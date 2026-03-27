pub mod redact_secrets;
pub mod semantic;

use std::any::Any;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, bounded};
use frankensqlite::compat::{ConnectionExt, ParamValue, RowExt};
use fs2::FileExt;
use notify::event::{AccessKind, AccessMode, MetadataKind, ModifyKind};
use notify::{RecursiveMode, Watcher, recommended_watcher};

use crate::connectors::{
    Connector, ScanRoot, aider::AiderConnector, amp::AmpConnector, chatgpt::ChatGptConnector,
    claude_code::ClaudeCodeConnector, clawdbot::ClawdbotConnector, cline::ClineConnector,
    codex::CodexConnector, copilot::CopilotConnector, copilot_cli::CopilotCliConnector,
    cursor::CursorConnector, factory::FactoryConnector, gemini::GeminiConnector,
    kimi::KimiConnector, openclaw::OpenClawConnector, opencode::OpenCodeConnector,
    pi_agent::PiAgentConnector, qwen::QwenConnector, vibe::VibeConnector,
};
use crate::connectors::{NormalizedConversation, NormalizedMessage};
use crate::search::tantivy::{TantivyIndex, index_dir, schema_hash_matches};
use crate::search::vector_index::{ROLE_ASSISTANT, ROLE_SYSTEM, ROLE_TOOL, ROLE_USER};

use crate::ensure_cass_origin;
use crate::sources::config::{Platform, SourcesConfig};
use crate::sources::provenance::{LOCAL_SOURCE_ID, Origin, Source, SourceKind};
use crate::sources::sync::path_to_safe_dirname;
use crate::storage::sqlite::{
    FrankenStorage, HistoricalSalvageOutcome, MigrationError,
    seed_canonical_from_best_historical_bundle,
};
use semantic::{EmbeddingInput, SemanticIndexer};

#[cfg(test)]
use std::iter::Peekable;

/// Type alias for batch classification map: (ConnectorKind, Path) -> (ScanRoot, MinTS, MaxTS)
type BatchClassificationMap =
    HashMap<(ConnectorKind, PathBuf), (ScanRoot, Option<i64>, Option<i64>)>;

fn message_id_from_db(raw: i64) -> Option<u64> {
    u64::try_from(raw).ok()
}

fn saturating_u32_from_i64(raw: i64) -> u32 {
    match u32::try_from(raw) {
        Ok(value) => value,
        Err(_) if raw.is_negative() => 0,
        Err(_) => u32::MAX,
    }
}

#[derive(Debug, Clone)]
pub enum ReindexCommand {
    Full,
}

#[derive(Debug)]
pub enum IndexerEvent {
    Notify(Vec<PathBuf>),
    Command(ReindexCommand),
}

// =============================================================================
// Stale Detection (Issue #54)
// =============================================================================

/// Action to take when watch daemon detects stale state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StaleAction {
    /// Log warning but take no automatic action.
    #[default]
    Warn,
    /// Automatically trigger full rebuild.
    Rebuild,
    /// Disable stale detection entirely.
    None,
}

impl StaleAction {
    /// Parse from environment variable value.
    fn from_env_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "rebuild" | "auto" | "fix" => Self::Rebuild,
            "none" | "off" | "disabled" | "0" | "false" => Self::None,
            _ => Self::Warn, // Default: warn, log
        }
    }
}

/// Configuration for stale detection.
#[derive(Debug, Clone)]
pub struct StaleConfig {
    /// Hours without successful ingests before considering stale (default: 24).
    pub threshold_hours: u64,
    /// Action to take when stale detected.
    pub action: StaleAction,
    /// Minutes between stale checks (default: 60).
    pub check_interval_mins: u64,
    /// Minimum scans with 0 conversations before triggering (default: 10).
    pub min_zero_scans: u64,
}

impl Default for StaleConfig {
    fn default() -> Self {
        Self {
            threshold_hours: 24,
            action: StaleAction::Warn,
            check_interval_mins: 60,
            min_zero_scans: 10,
        }
    }
}

impl StaleConfig {
    /// Load configuration from environment variables.
    pub fn from_env() -> Self {
        let mut cfg = Self::default();

        if let Ok(val) = dotenvy::var("CASS_WATCH_STALE_THRESHOLD_HOURS")
            && let Ok(hours) = val.parse()
        {
            cfg.threshold_hours = hours;
        }

        if let Ok(val) = dotenvy::var("CASS_WATCH_STALE_ACTION") {
            cfg.action = StaleAction::from_env_str(&val);
        }

        if let Ok(val) = dotenvy::var("CASS_WATCH_STALE_CHECK_INTERVAL_MINS")
            && let Ok(mins) = val.parse()
        {
            cfg.check_interval_mins = mins;
        }

        if let Ok(val) = dotenvy::var("CASS_WATCH_STALE_MIN_ZERO_SCANS")
            && let Ok(count) = val.parse()
        {
            cfg.min_zero_scans = count;
        }

        cfg
    }

    /// Check if stale detection is enabled.
    pub fn is_enabled(&self) -> bool {
        self.action != StaleAction::None
    }
}

/// Tracks indexing activity to detect stuck/stale states.
///
/// The watch daemon can get into a state where it runs but fails to index
/// new conversations (e.g., due to connector parsing issues). This detector
/// monitors ingest activity and triggers recovery when appropriate.
#[derive(Debug)]
pub struct StaleDetector {
    config: StaleConfig,
    /// Timestamp of last successful ingest (>0 conversations).
    last_successful_ingest: Mutex<Option<Instant>>,
    /// Count of consecutive scans with 0 conversations.
    consecutive_zero_scans: std::sync::atomic::AtomicU64,
    /// Whether stale warning has been emitted (to avoid spam).
    warning_emitted: AtomicBool,
    /// Last stale check timestamp.
    last_check: Mutex<Instant>,
    /// Total successful ingests since start.
    total_ingests: std::sync::atomic::AtomicU64,
    /// Time when the detector was created.
    start_time: Instant,
}

impl StaleDetector {
    /// Create a new stale detector with given configuration.
    pub fn new(config: StaleConfig) -> Self {
        Self {
            config,
            last_successful_ingest: Mutex::new(None),
            consecutive_zero_scans: std::sync::atomic::AtomicU64::new(0),
            warning_emitted: AtomicBool::new(false),
            last_check: Mutex::new(Instant::now()),
            total_ingests: std::sync::atomic::AtomicU64::new(0),
            start_time: Instant::now(),
        }
    }

    /// Create with configuration loaded from environment.
    pub fn from_env() -> Self {
        Self::new(StaleConfig::from_env())
    }

    /// Record a scan result.
    ///
    /// Called after each watch scan cycle with the number of conversations indexed.
    pub fn record_scan(&self, conversations_indexed: usize) {
        if conversations_indexed > 0 {
            // Successful ingest
            {
                let mut guard = self
                    .last_successful_ingest
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                *guard = Some(Instant::now());
            }
            self.consecutive_zero_scans.store(0, Ordering::Relaxed);
            self.warning_emitted.store(false, Ordering::Relaxed);
            self.total_ingests.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(
                conversations = conversations_indexed,
                "stale_detector: successful ingest recorded"
            );
        } else {
            // Zero-conversation scan
            let count = self.consecutive_zero_scans.fetch_add(1, Ordering::Relaxed) + 1;
            tracing::trace!(
                consecutive_zero_scans = count,
                "stale_detector: zero-conversation scan"
            );
        }
    }

    /// Check if the indexer appears to be in a stale state.
    ///
    /// Returns `Some(StaleAction)` if stale and action should be taken.
    pub fn check_stale(&self) -> Option<StaleAction> {
        if !self.config.is_enabled() {
            return None;
        }

        // Check if enough time has passed since last check
        let now = Instant::now();
        {
            let mut last_check = self.last_check.lock().unwrap_or_else(|e| e.into_inner());
            let check_interval = Duration::from_secs(self.config.check_interval_mins * 60);
            if now.duration_since(*last_check) < check_interval {
                return None;
            }
            *last_check = now;
        }

        // Check consecutive zero scans threshold
        let zero_count = self.consecutive_zero_scans.load(Ordering::Relaxed);
        if zero_count < self.config.min_zero_scans {
            return None;
        }

        // Check time since last successful ingest
        let threshold = Duration::from_secs(self.config.threshold_hours * 3600);
        let is_stale = match self
            .last_successful_ingest
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            Some(last) => now.duration_since(*last) > threshold,
            // No successful ingests ever - check if we've been running long enough
            None => now.duration_since(self.start_time) > threshold,
        };

        if is_stale {
            let already_warned = self.warning_emitted.swap(true, Ordering::Relaxed);
            if !already_warned || self.config.action == StaleAction::Rebuild {
                return Some(self.config.action);
            }
        }

        None
    }

    /// Get statistics for logging/debugging.
    pub fn stats(&self) -> StaleStats {
        let last_ingest = *self
            .last_successful_ingest
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        StaleStats {
            consecutive_zero_scans: self.consecutive_zero_scans.load(Ordering::Relaxed),
            total_ingests: self.total_ingests.load(Ordering::Relaxed),
            seconds_since_last_ingest: last_ingest.map(|t| t.elapsed().as_secs()),
            warning_emitted: self.warning_emitted.load(Ordering::Relaxed),
            config_action: format!("{:?}", self.config.action),
            config_threshold_hours: self.config.threshold_hours,
        }
    }

    /// Reset the detector state (e.g., after a full rebuild).
    pub fn reset(&self) {
        {
            let mut guard = self
                .last_successful_ingest
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *guard = Some(Instant::now());
        }
        self.consecutive_zero_scans.store(0, Ordering::Relaxed);
        self.warning_emitted.store(false, Ordering::Relaxed);
    }
}

/// Statistics from the stale detector.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StaleStats {
    pub consecutive_zero_scans: u64,
    pub total_ingests: u64,
    pub seconds_since_last_ingest: Option<u64>,
    pub warning_emitted: bool,
    pub config_action: String,
    pub config_threshold_hours: u64,
}

/// Per-connector statistics for structured logging (T7.4).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ConnectorStats {
    pub name: String,
    pub conversations: usize,
    pub messages: usize,
    pub scan_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Aggregate indexing statistics for JSON output (T7.4).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct IndexingStats {
    /// Time spent in scanning phase (ms)
    pub scan_ms: u64,
    /// Time spent in indexing phase (ms)
    pub index_ms: u64,
    /// Per-connector breakdown
    pub connectors: Vec<ConnectorStats>,
    /// Agents discovered during scan
    pub agents_discovered: Vec<String>,
    /// Total conversations indexed
    pub total_conversations: usize,
    /// Total messages indexed
    pub total_messages: usize,
}

#[derive(Debug, Default)]
pub struct IndexingProgress {
    pub total: AtomicUsize,
    pub current: AtomicUsize,
    // Simple phase indicator: 0=Idle, 1=Scanning, 2=Indexing
    pub phase: AtomicUsize,
    pub is_rebuilding: AtomicBool,
    /// Number of coding agents discovered so far during scanning
    pub discovered_agents: AtomicUsize,
    /// Names of discovered agents (protected by mutex for concurrent access)
    pub discovered_agent_names: Mutex<Vec<String>>,
    /// Last error message from background indexer, if any
    pub last_error: Mutex<Option<String>>,
    /// Structured stats for JSON output (T7.4)
    pub stats: Mutex<IndexingStats>,
}

#[derive(Clone)]
pub struct IndexOptions {
    pub full: bool,
    pub force_rebuild: bool,
    pub watch: bool,
    /// One-shot watch hook: when set, `watch_sources` will bypass notify and invoke reindex for these paths once.
    pub watch_once_paths: Option<Vec<PathBuf>>,
    pub db_path: PathBuf,
    pub data_dir: PathBuf,
    /// Build semantic vector index after text indexing.
    pub semantic: bool,
    /// Build HNSW index for approximate nearest neighbor search (requires semantic).
    pub build_hnsw: bool,
    /// Embedder ID to use for semantic indexing (hash, fastembed).
    pub embedder: String,
    pub progress: Option<Arc<IndexingProgress>>,
    /// Minimum interval (in seconds) between watch scan cycles. Prevents tight-loop
    /// CPU burn when filesystem events arrive continuously. Default: 30.
    pub watch_interval_secs: u64,
}

fn reset_progress_to_idle(progress: Option<&Arc<IndexingProgress>>) {
    let Some(progress) = progress else {
        return;
    };

    progress.phase.store(0, Ordering::Relaxed);
    progress.is_rebuilding.store(false, Ordering::Relaxed);
}

struct RunIndexProgressReset {
    progress: Option<Arc<IndexingProgress>>,
}

impl RunIndexProgressReset {
    fn new(progress: Option<Arc<IndexingProgress>>) -> Self {
        Self { progress }
    }
}

impl Drop for RunIndexProgressReset {
    fn drop(&mut self) {
        reset_progress_to_idle(self.progress.as_ref());
    }
}

const LEXICAL_REBUILD_STATE_VERSION: u8 = 2;
const LEXICAL_REBUILD_PAGE_SIZE: i64 = 200;

#[derive(Debug)]
struct IndexRunLockGuard {
    // Keep the file handle alive for the lifetime of the lock.
    _file: File,
    _path: PathBuf,
}

impl Drop for IndexRunLockGuard {
    fn drop(&mut self) {
        let _ = self._file.unlock();
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LexicalRebuildDbState {
    db_path: String,
    total_conversations: usize,
    storage_fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingLexicalCommit {
    next_offset: i64,
    processed_conversations: usize,
    indexed_docs: usize,
    base_meta_fingerprint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LexicalRebuildState {
    version: u8,
    schema_hash: String,
    db: LexicalRebuildDbState,
    page_size: i64,
    committed_offset: i64,
    processed_conversations: usize,
    indexed_docs: usize,
    committed_meta_fingerprint: Option<String>,
    pending: Option<PendingLexicalCommit>,
    completed: bool,
    updated_at_ms: i64,
}

impl LexicalRebuildState {
    fn new(db: LexicalRebuildDbState, page_size: i64) -> Self {
        Self {
            version: LEXICAL_REBUILD_STATE_VERSION,
            schema_hash: crate::search::tantivy::SCHEMA_HASH.to_string(),
            db,
            page_size,
            committed_offset: 0,
            processed_conversations: 0,
            indexed_docs: 0,
            committed_meta_fingerprint: None,
            pending: None,
            completed: false,
            updated_at_ms: FrankenStorage::now_millis(),
        }
    }

    fn matches_run(&self, db: &LexicalRebuildDbState, page_size: i64) -> bool {
        self.version == LEXICAL_REBUILD_STATE_VERSION
            && self.schema_hash == crate::search::tantivy::SCHEMA_HASH
            && &self.db == db
            && self.page_size == page_size
    }

    fn record_pending_commit(
        &mut self,
        next_offset: i64,
        processed_conversations: usize,
        indexed_docs: usize,
        base_meta_fingerprint: Option<String>,
    ) {
        self.pending = Some(PendingLexicalCommit {
            next_offset,
            processed_conversations,
            indexed_docs,
            base_meta_fingerprint,
        });
        self.completed = false;
        self.updated_at_ms = FrankenStorage::now_millis();
    }

    fn finalize_commit(&mut self, committed_meta_fingerprint: Option<String>) {
        if let Some(pending) = self.pending.take() {
            self.committed_offset = pending.next_offset;
            self.processed_conversations = pending.processed_conversations;
            self.indexed_docs = pending.indexed_docs;
        }
        self.committed_meta_fingerprint = committed_meta_fingerprint;
        self.completed = false;
        self.updated_at_ms = FrankenStorage::now_millis();
    }

    fn clear_pending(&mut self) {
        self.pending = None;
        self.updated_at_ms = FrankenStorage::now_millis();
    }

    fn mark_completed(&mut self, committed_meta_fingerprint: Option<String>) {
        self.committed_meta_fingerprint = committed_meta_fingerprint;
        self.pending = None;
        self.completed = true;
        self.updated_at_ms = FrankenStorage::now_millis();
    }

    fn is_incomplete(&self) -> bool {
        !self.completed
    }
}

fn acquire_index_run_lock(data_dir: &Path, db_path: &Path) -> Result<IndexRunLockGuard> {
    fs::create_dir_all(data_dir)
        .with_context(|| format!("creating cass data directory {}", data_dir.display()))?;
    let lock_path = data_dir.join("index-run.lock");
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("opening index-run lock file {}", lock_path.display()))?;

    file.try_lock_exclusive().with_context(|| {
        format!(
            "another cass index process already holds {}",
            lock_path.display()
        )
    })?;

    file.set_len(0).with_context(|| {
        format!(
            "truncating index-run lock file after acquisition: {}",
            lock_path.display()
        )
    })?;
    writeln!(
        file,
        "pid={}\nstarted_at_ms={}\ndb_path={}",
        std::process::id(),
        FrankenStorage::now_millis(),
        db_path.display()
    )
    .with_context(|| format!("writing index-run metadata to {}", lock_path.display()))?;
    file.flush()
        .with_context(|| format!("flushing index-run lock file {}", lock_path.display()))?;

    Ok(IndexRunLockGuard {
        _file: file,
        _path: lock_path,
    })
}

fn lexical_rebuild_state_path(index_path: &Path) -> PathBuf {
    index_path.join(".lexical-rebuild-state.json")
}

fn lexical_rebuild_commit_interval_conversations() -> usize {
    dotenvy::var("CASS_TANTIVY_REBUILD_COMMIT_EVERY_CONVERSATIONS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(1_000)
}

fn lexical_rebuild_commit_interval_messages() -> usize {
    dotenvy::var("CASS_TANTIVY_REBUILD_COMMIT_EVERY_MESSAGES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(5_000)
}

fn lexical_rebuild_commit_interval_message_bytes() -> usize {
    dotenvy::var("CASS_TANTIVY_REBUILD_COMMIT_EVERY_MESSAGE_BYTES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(16 * 1024 * 1024)
}

fn should_commit_lexical_rebuild(
    conversations_since_commit: usize,
    messages_since_commit: usize,
    message_bytes_since_commit: usize,
    commit_interval_conversations: usize,
    commit_interval_messages: usize,
    commit_interval_message_bytes: usize,
) -> bool {
    conversations_since_commit >= commit_interval_conversations
        || messages_since_commit >= commit_interval_messages
        || message_bytes_since_commit >= commit_interval_message_bytes
}

fn write_json_pretty_atomically<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating parent directory for {}", path.display()))?;
    }
    let temp_path = unique_atomic_temp_path(path);
    {
        let file = File::create(&temp_path)
            .with_context(|| format!("creating temporary file {}", temp_path.display()))?;
        let mut writer = BufWriter::new(file);
        serde_json::to_writer_pretty(&mut writer, value)
            .with_context(|| format!("serializing {}", path.display()))?;
        writer
            .flush()
            .with_context(|| format!("flushing temporary file {}", temp_path.display()))?;
    }
    replace_file_from_temp(&temp_path, path)
        .with_context(|| format!("replacing {} from temp file", path.display()))
}

fn load_lexical_rebuild_state(index_path: &Path) -> Result<Option<LexicalRebuildState>> {
    let path = lexical_rebuild_state_path(index_path);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("reading lexical rebuild state {}", path.display()));
        }
    };

    match serde_json::from_slice::<LexicalRebuildState>(&bytes) {
        Ok(state) => Ok(Some(state)),
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "ignoring malformed lexical rebuild checkpoint"
            );
            Ok(None)
        }
    }
}

fn persist_lexical_rebuild_state(index_path: &Path, state: &LexicalRebuildState) -> Result<()> {
    let path = lexical_rebuild_state_path(index_path);
    write_json_pretty_atomically(&path, state)
}

fn clear_lexical_rebuild_state(index_path: &Path) -> Result<()> {
    let path = lexical_rebuild_state_path(index_path);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => {
            Err(err).with_context(|| format!("removing lexical rebuild state {}", path.display()))
        }
    }
}

fn index_meta_fingerprint(index_path: &Path) -> Result<Option<String>> {
    let meta_path = index_path.join("meta.json");
    match fs::read(&meta_path) {
        Ok(bytes) => Ok(Some(blake3::hash(&bytes).to_hex().to_string())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => {
            Err(err).with_context(|| format!("reading Tantivy meta file {}", meta_path.display()))
        }
    }
}

fn pending_commit_landed(
    base_meta_fingerprint: Option<&str>,
    current_meta_fingerprint: Option<&str>,
) -> bool {
    match (base_meta_fingerprint, current_meta_fingerprint) {
        (None, Some(_)) => true,
        (Some(base), Some(current)) => current != base,
        _ => false,
    }
}

fn reconcile_pending_lexical_commit(
    index_path: &Path,
    mut state: LexicalRebuildState,
) -> Result<LexicalRebuildState> {
    let Some(pending) = state.pending.clone() else {
        return Ok(state);
    };

    let current_meta_fingerprint = index_meta_fingerprint(index_path)?;
    if pending_commit_landed(
        pending.base_meta_fingerprint.as_deref(),
        current_meta_fingerprint.as_deref(),
    ) {
        state.finalize_commit(current_meta_fingerprint);
    } else {
        state.clear_pending();
    }
    persist_lexical_rebuild_state(index_path, &state)?;
    Ok(state)
}

fn metadata_stamp(path: &Path) -> Result<(u64, i64)> {
    let metadata =
        fs::metadata(path).with_context(|| format!("reading metadata for {}", path.display()))?;
    let modified_ms = metadata
        .modified()
        .ok()
        .and_then(|ts| ts.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|dur| i64::try_from(dur.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    Ok((metadata.len(), modified_ms))
}

fn lexical_rebuild_storage_fingerprint(db_path: &Path) -> Result<String> {
    let wal_path = PathBuf::from(format!("{}-wal", db_path.display()));
    let (db_len, db_mtime_ms) = metadata_stamp(db_path)?;
    let (wal_len, wal_mtime_ms) = match fs::metadata(&wal_path) {
        Ok(_) => metadata_stamp(&wal_path)?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => (0, 0),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("reading metadata for {}", wal_path.display()));
        }
    };
    Ok(format!("{db_len}:{db_mtime_ms}:{wal_len}:{wal_mtime_ms}"))
}

fn count_total_conversations_exact(storage: &FrankenStorage) -> Result<usize> {
    let total_conversations: i64 = storage
        .raw()
        .query_row_map(
            "SELECT COUNT(*) FROM conversations",
            &[] as &[ParamValue],
            |row| row.get_typed(0),
        )
        .context("counting canonical conversations for lexical rebuild state")?;
    Ok(usize::try_from(total_conversations.max(0)).unwrap_or(usize::MAX))
}

fn should_salvage_historical_databases(
    storage_rebuilt: bool,
    canonical_sessions_before_salvage: usize,
    has_pending_historical_bundles: bool,
    canonical_only_full_rebuild: bool,
) -> bool {
    if canonical_only_full_rebuild {
        return false;
    }
    storage_rebuilt || canonical_sessions_before_salvage == 0 || has_pending_historical_bundles
}

fn count_meta_entries_like(storage: &FrankenStorage, pattern: &str) -> Result<i64> {
    storage
        .raw()
        .query_row_map(
            "SELECT COUNT(*) FROM meta WHERE key LIKE ?1",
            &[ParamValue::from(pattern)],
            |row| row.get_typed(0),
        )
        .context(format!("counting meta rows matching {pattern}"))
}

fn full_rebuild_requires_historical_restart(
    storage: &FrankenStorage,
    db_path: &Path,
    canonical_sessions_before_salvage: usize,
) -> Result<bool> {
    let bundles = crate::storage::sqlite::discover_historical_database_bundles(db_path);
    if bundles.is_empty() {
        return Ok(false);
    }

    let in_progress = count_meta_entries_like(storage, "historical_bundle_progress:%")? > 0;
    if in_progress && canonical_sessions_before_salvage > 0 {
        tracing::warn!(
            db_path = %db_path.display(),
            "ignoring stale historical salvage progress markers because the canonical database is already healthy and populated"
        );
    }

    // Do not compare MAX(messages.id) across separate SQLite files. Those ids are only local
    // row identifiers inside each database, so using them as a global freshness watermark
    // produces false positives and can cause a healthy canonical database to be replaced.
    //
    // If historical bundles still contain unique conversations/messages, incremental salvage
    // should import that delta into the populated canonical database without resetting it.
    Ok(false)
}

fn lexical_rebuild_db_state(
    storage: &FrankenStorage,
    db_path: &Path,
) -> Result<LexicalRebuildDbState> {
    Ok(LexicalRebuildDbState {
        db_path: db_path.to_string_lossy().into_owned(),
        total_conversations: count_total_conversations_exact(storage)?,
        storage_fingerprint: lexical_rebuild_storage_fingerprint(db_path)?,
    })
}

fn has_pending_lexical_rebuild(
    index_path: &Path,
    db_state: &LexicalRebuildDbState,
) -> Result<bool> {
    let Some(state) = load_lexical_rebuild_state(index_path)? else {
        return Ok(false);
    };
    Ok(state.matches_run(db_state, LEXICAL_REBUILD_PAGE_SIZE) && state.is_incomplete())
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct LexicalRebuildSnapshot {
    pub db_path: String,
    pub total_conversations: usize,
    pub storage_fingerprint: String,
    pub committed_offset: i64,
    pub processed_conversations: usize,
    pub indexed_docs: usize,
    pub completed: bool,
    pub updated_at_ms: i64,
}

pub(crate) fn load_lexical_rebuild_snapshot(
    index_path: &Path,
    db_path: &Path,
) -> Result<Option<LexicalRebuildSnapshot>> {
    let Some(state) = load_lexical_rebuild_state(index_path)? else {
        return Ok(None);
    };

    if state.completed || state.db.db_path != db_path.to_string_lossy() {
        return Ok(None);
    }

    Ok(Some(LexicalRebuildSnapshot {
        db_path: state.db.db_path,
        total_conversations: state.db.total_conversations,
        storage_fingerprint: state.db.storage_fingerprint,
        committed_offset: state.committed_offset,
        processed_conversations: state.processed_conversations,
        indexed_docs: state.indexed_docs,
        completed: state.completed,
        updated_at_ms: state.updated_at_ms,
    }))
}

fn repair_daily_stats_if_drifted(storage: &FrankenStorage, db_path: &Path) -> Result<()> {
    let health = storage.daily_stats_health().with_context(|| {
        format!(
            "checking daily_stats health before index planning for {}",
            db_path.display()
        )
    })?;

    if health.populated && health.drift == 0 {
        return Ok(());
    }

    tracing::warn!(
        db_path = %db_path.display(),
        populated = health.populated,
        row_count = health.row_count,
        conversation_count = health.conversation_count,
        materialized_total = health.materialized_total,
        drift = health.drift,
        "daily_stats is missing or drifted; rebuilding from canonical conversations"
    );

    let rebuilt = storage.rebuild_daily_stats().with_context(|| {
        format!(
            "rebuilding daily_stats before index planning for {}",
            db_path.display()
        )
    })?;

    tracing::info!(
        db_path = %db_path.display(),
        rows_created = rebuilt.rows_created,
        total_sessions = rebuilt.total_sessions,
        "rebuilt daily_stats before index planning"
    );

    Ok(())
}

// =============================================================================
// Streaming Indexing (Opt 8.2)
// =============================================================================

/// Message type for streaming indexing channel.
///
/// Producers (connector scan threads) send batches of conversations through
/// the channel. The consumer (main indexing thread) receives and ingests them.
pub enum IndexMessage {
    /// A batch of conversations from a connector scan.
    Batch {
        /// Connector name (e.g., "claude", "codex")
        connector_name: &'static str,
        /// Scanned conversations
        conversations: Vec<NormalizedConversation>,
        /// Whether this connector was newly discovered
        is_discovered: bool,
        /// Message count in this batch (for stats)
        message_count: usize,
        /// Reserved text-byte budget for this batch that must be released after ingestion.
        byte_reservation: usize,
    },
    /// A scan error occurred (non-fatal, logged but continues)
    ScanError {
        connector_name: &'static str,
        error: String,
    },
    /// Producer has finished scanning
    Done {
        connector_name: &'static str,
        /// Time spent scanning this connector (ms)
        scan_ms: u64,
        /// Whether this connector was discovered even if it produced no batches
        is_discovered: bool,
    },
}

/// Default channel buffer size for streaming indexing.
/// Balances memory usage with throughput - too small causes producer stalls,
/// too large defeats the purpose of backpressure.
const STREAMING_CHANNEL_SIZE: usize = 32;

#[derive(Debug, Clone, Copy)]
struct StreamingBatchLimits {
    max_conversations: usize,
    max_messages: usize,
    max_chars: usize,
}

const DEFAULT_STREAMING_BATCH_LIMITS: StreamingBatchLimits = StreamingBatchLimits {
    max_conversations: 64,
    max_messages: 2_000,
    max_chars: 4 * 1024 * 1024,
};

/// Maximum total text bytes allowed across queued/in-flight streaming batches.
///
/// This preserves the intended memory envelope for normal batches while also
/// preventing oversized single conversations from multiplying across the queue.
const STREAMING_MAX_BYTES_IN_FLIGHT: usize =
    STREAMING_CHANNEL_SIZE * DEFAULT_STREAMING_BATCH_LIMITS.max_chars;

#[derive(Debug)]
struct StreamingByteLimiterState {
    bytes_in_flight: usize,
    closed: bool,
}

#[derive(Debug)]
struct StreamingByteLimiter {
    max_bytes_in_flight: usize,
    state: Mutex<StreamingByteLimiterState>,
    cv: Condvar,
}

impl StreamingByteLimiter {
    fn new(max_bytes_in_flight: usize) -> Self {
        debug_assert!(max_bytes_in_flight > 0);
        Self {
            max_bytes_in_flight,
            state: Mutex::new(StreamingByteLimiterState {
                bytes_in_flight: 0,
                closed: false,
            }),
            cv: Condvar::new(),
        }
    }

    fn acquire(&self, requested_bytes: usize) -> Result<usize> {
        if requested_bytes == 0 {
            return Ok(0);
        }

        let reservation = requested_bytes.min(self.max_bytes_in_flight);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if state.closed {
                return Err(anyhow::anyhow!(
                    "streaming byte limiter closed while waiting for capacity"
                ));
            }

            if state.bytes_in_flight.saturating_add(reservation) <= self.max_bytes_in_flight {
                state.bytes_in_flight += reservation;
                return Ok(reservation);
            }

            state = self.cv.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn release(&self, reserved_bytes: usize) {
        if reserved_bytes == 0 {
            return;
        }

        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.bytes_in_flight = state.bytes_in_flight.saturating_sub(reserved_bytes);
        self.cv.notify_all();
    }

    fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.closed = true;
        self.cv.notify_all();
    }
}

fn conversation_batch_footprint(conv: &NormalizedConversation) -> (usize, usize) {
    let message_count = conv.messages.len();
    let char_count = conv.messages.iter().map(|msg| msg.content.len()).sum();
    (message_count, char_count)
}

#[cfg(test)]
fn next_streaming_batch(
    conversations: &mut Peekable<std::vec::IntoIter<NormalizedConversation>>,
    limits: StreamingBatchLimits,
) -> Option<(Vec<NormalizedConversation>, usize)> {
    let first = conversations.next()?;
    let (first_messages, first_chars) = conversation_batch_footprint(&first);
    let mut batch = vec![first];
    let mut total_messages = first_messages;
    let mut total_chars = first_chars;

    while let Some(next) = conversations.peek() {
        let (next_messages, next_chars) = conversation_batch_footprint(next);
        let would_exceed_limits = batch.len() >= limits.max_conversations
            || total_messages.saturating_add(next_messages) > limits.max_messages
            || total_chars.saturating_add(next_chars) > limits.max_chars;
        if would_exceed_limits {
            break;
        }

        let conv = conversations
            .next()
            .expect("peek indicated another conversation existed");
        total_messages += next_messages;
        total_chars += next_chars;
        batch.push(conv);
    }

    Some((batch, total_messages))
}

struct StreamingBatchSender<'a> {
    tx: &'a Sender<IndexMessage>,
    flow_limiter: Arc<StreamingByteLimiter>,
    connector_name: &'static str,
    next_batch_is_discovered: bool,
    conversations: Vec<NormalizedConversation>,
    message_count: usize,
    char_count: usize,
}

fn remember_discovered_connector(discovered_names: &mut Vec<String>, connector_name: &'static str) {
    if !discovered_names.iter().any(|name| name == connector_name) {
        discovered_names.push(connector_name.to_string());
    }
}

impl<'a> StreamingBatchSender<'a> {
    fn new(
        tx: &'a Sender<IndexMessage>,
        flow_limiter: Arc<StreamingByteLimiter>,
        connector_name: &'static str,
        is_discovered: bool,
    ) -> Self {
        Self {
            tx,
            flow_limiter,
            connector_name,
            next_batch_is_discovered: is_discovered,
            conversations: Vec::new(),
            message_count: 0,
            char_count: 0,
        }
    }

    fn mark_next_batch_discovered(&mut self) {
        self.next_batch_is_discovered = true;
    }

    fn push(&mut self, conversation: NormalizedConversation) -> Result<()> {
        let (message_count, char_count) = conversation_batch_footprint(&conversation);
        let would_exceed_limits = !self.conversations.is_empty()
            && (self.conversations.len() >= DEFAULT_STREAMING_BATCH_LIMITS.max_conversations
                || self.message_count.saturating_add(message_count)
                    > DEFAULT_STREAMING_BATCH_LIMITS.max_messages
                || self.char_count.saturating_add(char_count)
                    > DEFAULT_STREAMING_BATCH_LIMITS.max_chars);
        if would_exceed_limits {
            self.flush()?;
        }

        self.message_count += message_count;
        self.char_count += char_count;
        self.conversations.push(conversation);

        let single_conversation_exceeds_limits = self.conversations.len() == 1
            && (self.message_count > DEFAULT_STREAMING_BATCH_LIMITS.max_messages
                || self.char_count > DEFAULT_STREAMING_BATCH_LIMITS.max_chars);
        if single_conversation_exceeds_limits {
            self.flush()?;
        }

        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if self.conversations.is_empty() {
            return Ok(());
        }

        let byte_reservation = self.flow_limiter.acquire(self.char_count).map_err(|_| {
            anyhow::Error::new(StreamingConsumerDisconnected {
                connector_name: self.connector_name,
            })
        })?;
        let message_count = self.message_count;
        let conversations = std::mem::take(&mut self.conversations);
        if let Err(_send_error) = self.tx.send(IndexMessage::Batch {
            connector_name: self.connector_name,
            conversations,
            is_discovered: self.next_batch_is_discovered,
            message_count,
            byte_reservation,
        }) {
            self.flow_limiter.release(byte_reservation);
            return Err(anyhow::Error::new(StreamingConsumerDisconnected {
                connector_name: self.connector_name,
            }));
        }
        self.message_count = 0;
        self.char_count = 0;
        self.next_batch_is_discovered = false;
        Ok(())
    }
}

#[cfg(test)]
fn send_conversation_batches(
    tx: &Sender<IndexMessage>,
    connector_name: &'static str,
    conversations: Vec<NormalizedConversation>,
    is_discovered: bool,
) {
    let mut sender = StreamingBatchSender::new(
        tx,
        Arc::new(StreamingByteLimiter::new(STREAMING_MAX_BYTES_IN_FLIGHT)),
        connector_name,
        is_discovered,
    );
    for conversation in conversations {
        sender
            .push(conversation)
            .expect("test batch sender should deliver to in-memory receiver");
    }
    sender
        .flush()
        .expect("test batch sender should flush to in-memory receiver");
}

/// Check if streaming indexing is enabled via environment variable.
///
/// Set `CASS_STREAMING_INDEX=0` to disable streaming and use batch mode.
/// Streaming is enabled by default.
pub fn streaming_index_enabled() -> bool {
    dotenvy::var("CASS_STREAMING_INDEX")
        .map(|v| !(v == "0" || v.eq_ignore_ascii_case("false")))
        .unwrap_or(true)
}

fn panic_payload_message(payload: Box<dyn Any + Send>) -> String {
    match payload.downcast::<String>() {
        Ok(message) => *message,
        Err(payload) => match payload.downcast::<&'static str>() {
            Ok(message) => (*message).to_string(),
            Err(_) => "non-string panic payload".to_string(),
        },
    }
}

#[derive(Debug)]
struct StreamingConsumerDisconnected {
    connector_name: &'static str,
}

impl std::fmt::Display for StreamingConsumerDisconnected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "streaming consumer disconnected while sending batch for {}",
            self.connector_name
        )
    }
}

impl std::error::Error for StreamingConsumerDisconnected {}

fn is_streaming_consumer_disconnected(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<StreamingConsumerDisconnected>()
        .is_some()
}

#[derive(Clone)]
struct StreamingProducerConfig {
    flow_limiter: Arc<StreamingByteLimiter>,
    data_dir: PathBuf,
    remote_roots: Vec<ScanRoot>,
    since_ts: Option<i64>,
    progress: Option<Arc<IndexingProgress>>,
}

/// Spawn a producer thread that scans a connector and sends batches through the channel.
///
/// Each connector runs in its own thread, scanning local and remote roots.
/// Conversations are sent through the channel as they're discovered, providing
/// backpressure when the consumer (indexer) falls behind.
fn spawn_connector_producer(
    name: &'static str,
    factory: fn() -> Box<dyn Connector + Send>,
    tx: Sender<IndexMessage>,
    config: StreamingProducerConfig,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let scan_start = std::time::Instant::now();
        let conn = factory();
        let detect = conn.detect();
        let was_detected = detect.detected;
        let mut is_discovered = false;

        if detect.detected {
            // Update discovered agents count immediately when detected
            if let Some(p) = &config.progress {
                p.discovered_agents.fetch_add(1, Ordering::Relaxed);
            }
            is_discovered = true;

            // Scan local sources
            let ctx = crate::connectors::ScanContext::local_default(
                config.data_dir.clone(),
                config.since_ts,
            );
            let local_origin = Origin::local();
            let mut batch_sender =
                StreamingBatchSender::new(&tx, config.flow_limiter.clone(), name, is_discovered);
            match conn.scan_with_callback(&ctx, &mut |mut conversation| {
                inject_provenance(&mut conversation, &local_origin);
                batch_sender.push(conversation)
            }) {
                Ok(()) => {
                    if let Err(error) = batch_sender.flush() {
                        if is_streaming_consumer_disconnected(&error) {
                            tracing::info!(
                                connector = name,
                                "streaming consumer disconnected; stopping producer"
                            );
                            return;
                        }
                        tracing::warn!(connector = name, "local flush failed: {}", error);
                        let _ = tx.send(IndexMessage::ScanError {
                            connector_name: name,
                            error: format!("local flush failed: {error}"),
                        });
                    }
                }
                Err(e) => {
                    if let Err(flush_error) = batch_sender.flush()
                        && !is_streaming_consumer_disconnected(&flush_error)
                    {
                        tracing::warn!(connector = name, "local flush failed: {}", flush_error);
                    }
                    if is_streaming_consumer_disconnected(&e) {
                        tracing::info!(
                            connector = name,
                            "streaming consumer disconnected; stopping producer"
                        );
                        return;
                    }
                    tracing::warn!(connector = name, "local scan failed: {}", e);
                    let _ = tx.send(IndexMessage::ScanError {
                        connector_name: name,
                        error: e.to_string(),
                    });
                }
            }
        }

        // Scan remote sources
        for root in &config.remote_roots {
            let ctx = crate::connectors::ScanContext::with_roots(
                root.path.clone(),
                vec![root.clone()],
                config.since_ts,
            );
            let mut batch_sender =
                StreamingBatchSender::new(&tx, config.flow_limiter.clone(), name, is_discovered);
            match conn.scan_with_callback(&ctx, &mut |mut conversation| {
                inject_provenance(&mut conversation, &root.origin);
                apply_workspace_rewrite(&mut conversation, root);

                if !was_detected && !is_discovered {
                    if let Some(p) = &config.progress {
                        p.discovered_agents.fetch_add(1, Ordering::Relaxed);
                    }
                    is_discovered = true;
                    batch_sender.mark_next_batch_discovered();
                }

                batch_sender.push(conversation)
            }) {
                Ok(()) => {
                    if let Err(error) = batch_sender.flush() {
                        if is_streaming_consumer_disconnected(&error) {
                            tracing::info!(
                                connector = name,
                                "streaming consumer disconnected; stopping producer"
                            );
                            return;
                        }
                        tracing::warn!(
                            connector = name,
                            root = %root.path.display(),
                            "remote flush failed: {}",
                            error
                        );
                        let _ = tx.send(IndexMessage::ScanError {
                            connector_name: name,
                            error: format!(
                                "remote flush failed for {}: {}",
                                root.path.display(),
                                error
                            ),
                        });
                    }
                }
                Err(e) => {
                    if let Err(flush_error) = batch_sender.flush()
                        && !is_streaming_consumer_disconnected(&flush_error)
                    {
                        tracing::warn!(
                            connector = name,
                            root = %root.path.display(),
                            "remote flush failed: {}",
                            flush_error
                        );
                    }
                    if is_streaming_consumer_disconnected(&e) {
                        tracing::info!(
                            connector = name,
                            "streaming consumer disconnected; stopping producer"
                        );
                        return;
                    }
                    tracing::warn!(
                        connector = name,
                        root = %root.path.display(),
                        "remote scan failed: {}", e
                    );
                    let _ = tx.send(IndexMessage::ScanError {
                        connector_name: name,
                        error: format!("remote scan failed for {}: {}", root.path.display(), e),
                    });
                }
            }
        }

        let scan_ms = scan_start.elapsed().as_millis() as u64;
        tracing::info!(
            connector = name,
            discovered = is_discovered,
            scan_ms,
            "streaming_scan_complete"
        );

        // Signal completion with timing
        let _ = tx.send(IndexMessage::Done {
            connector_name: name,
            scan_ms,
            is_discovered,
        });
    })
}

/// Run the streaming indexing consumer.
///
/// Receives batches from producer threads and ingests them into storage.
/// Processes batches as they arrive, providing early feedback and reducing
/// peak memory usage compared to batch collection.
fn run_streaming_consumer(
    rx: Receiver<IndexMessage>,
    num_producers: usize,
    storage: &FrankenStorage,
    t_index: &mut TantivyIndex,
    flow_limiter: Arc<StreamingByteLimiter>,
    progress: &Option<Arc<IndexingProgress>>,
    needs_rebuild: bool,
) -> Result<Vec<String>> {
    use std::collections::HashMap;

    let mut active_producers = num_producers;
    let mut discovered_names: Vec<String> = Vec::new();
    let mut total_conversations = 0usize;
    let mut total_messages = 0usize;
    let mut switched_to_indexing = false;
    let mut last_commit = std::time::Instant::now();
    let index_start = std::time::Instant::now();

    // Per-connector stats tracking (T7.4)
    let mut connector_stats: HashMap<String, ConnectorStats> = HashMap::new();

    loop {
        match rx.recv() {
            Ok(IndexMessage::Batch {
                connector_name,
                conversations,
                is_discovered,
                message_count,
                byte_reservation,
            }) => {
                let batch_size = conversations.len();
                total_conversations += batch_size;
                total_messages += message_count;

                // Update per-connector stats
                let stats = connector_stats
                    .entry(connector_name.to_string())
                    .or_insert_with(|| ConnectorStats {
                        name: connector_name.to_string(),
                        ..Default::default()
                    });
                stats.conversations += batch_size;
                stats.messages += message_count;

                // Switch to indexing phase on first batch (reset total/current for accurate progress)
                if !switched_to_indexing {
                    if let Some(p) = progress {
                        p.phase.store(2, Ordering::Relaxed); // Indexing
                        p.total.store(0, Ordering::Relaxed); // Reset - will accumulate as batches arrive
                        p.current.store(0, Ordering::Relaxed);
                    }
                    switched_to_indexing = true;
                }

                // Update progress total (we learn about sizes as batches arrive)
                if let Some(p) = progress {
                    p.total.fetch_add(batch_size, Ordering::Relaxed);
                }

                // Track discovered agent names
                if is_discovered {
                    remember_discovered_connector(&mut discovered_names, connector_name);
                }

                // Ingest the batch
                let ingest_result =
                    ingest_batch(storage, t_index, &conversations, progress, needs_rebuild);
                flow_limiter.release(byte_reservation);
                ingest_result?;

                // Periodic commit to make results visible incrementally (every 5s)
                if last_commit.elapsed() >= Duration::from_secs(5) {
                    if let Err(e) = t_index.commit() {
                        tracing::warn!("incremental commit failed: {}", e);
                    } else {
                        tracing::debug!("incremental commit completed");
                    }
                    last_commit = std::time::Instant::now();
                }

                tracing::info!(
                    connector = connector_name,
                    conversations = batch_size,
                    messages = message_count,
                    "streaming_ingest"
                );
            }
            Ok(IndexMessage::ScanError {
                connector_name,
                error,
            }) => {
                // Record error in connector stats
                let stats = connector_stats
                    .entry(connector_name.to_string())
                    .or_insert_with(|| ConnectorStats {
                        name: connector_name.to_string(),
                        ..Default::default()
                    });
                stats.error = Some(error.clone());

                tracing::warn!(
                    connector = connector_name,
                    error = %error,
                    "streaming_scan_error"
                );
                // Continue processing - scan errors are non-fatal
            }
            Ok(IndexMessage::Done {
                connector_name,
                scan_ms,
                is_discovered,
            }) => {
                active_producers -= 1;

                // Record scan timing in connector stats
                let stats = connector_stats
                    .entry(connector_name.to_string())
                    .or_insert_with(|| ConnectorStats {
                        name: connector_name.to_string(),
                        ..Default::default()
                    });
                stats.scan_ms = scan_ms;

                if is_discovered {
                    remember_discovered_connector(&mut discovered_names, connector_name);
                }

                // If we haven't switched to indexing phase yet, this Done message represents
                // a completed scan step. Increment current to show scanning progress.
                if !switched_to_indexing && let Some(p) = progress {
                    p.current.fetch_add(1, Ordering::Relaxed);
                }

                tracing::debug!(
                    connector = connector_name,
                    scan_ms,
                    remaining = active_producers,
                    "streaming_producer_done"
                );
                if active_producers == 0 {
                    break;
                }
            }
            Err(_) => {
                let error = format!(
                    "streaming indexing aborted: channel closed with {active_producers} producers still active"
                );
                tracing::warn!(remaining = active_producers, error = %error);
                set_progress_last_error(progress.as_ref(), Some(error.clone()));
                return Err(anyhow::anyhow!(error));
            }
        }
    }

    // Final commit to ensure all data is persisted
    t_index.commit()?;

    let index_ms = index_start.elapsed().as_millis() as u64;

    // Calculate total scan time (max of all connector scan times since they run in parallel)
    let scan_ms = connector_stats
        .values()
        .map(|s| s.scan_ms)
        .max()
        .unwrap_or(0);

    // Update progress with final stats (T7.4)
    if let Some(p) = progress
        && let Ok(mut stats) = p.stats.lock()
    {
        stats.scan_ms = scan_ms;
        stats.index_ms = index_ms;
        stats.connectors = connector_stats.values().cloned().collect();
        stats.agents_discovered = discovered_names.clone();
        stats.total_conversations = total_conversations;
        stats.total_messages = total_messages;
    }

    tracing::info!(
        total_conversations,
        total_messages,
        scan_ms,
        index_ms,
        discovered = discovered_names.len(),
        "streaming_indexing_complete"
    );

    Ok(discovered_names)
}

/// Run indexing using streaming architecture with backpressure.
///
/// This spawns producer threads for each connector that send batches through
/// a bounded channel. The consumer receives and ingests batches as they arrive,
/// providing backpressure when indexing falls behind scanning.
fn run_streaming_index(
    storage: &FrankenStorage,
    t_index: &mut TantivyIndex,
    opts: &IndexOptions,
    since_ts: Option<i64>,
    needs_rebuild: bool,
    remote_roots: Vec<ScanRoot>,
) -> Result<()> {
    run_streaming_index_with_connector_factories(
        storage,
        t_index,
        opts,
        since_ts,
        needs_rebuild,
        remote_roots,
        get_connector_factories(),
    )
}

type ConnectorFactory = fn() -> Box<dyn Connector + Send>;

fn run_streaming_index_with_connector_factories(
    storage: &FrankenStorage,
    t_index: &mut TantivyIndex,
    opts: &IndexOptions,
    since_ts: Option<i64>,
    needs_rebuild: bool,
    remote_roots: Vec<ScanRoot>,
    connector_factories: Vec<(&'static str, ConnectorFactory)>,
) -> Result<()> {
    let buffered_connectors: Vec<&'static str> = connector_factories
        .iter()
        .filter_map(|(name, factory)| {
            let connector = factory();
            (!connector.supports_streaming_scan()).then_some(*name)
        })
        .collect();
    let num_connectors = connector_factories.len();

    if !buffered_connectors.is_empty() {
        tracing::warn!(
            connectors = ?buffered_connectors,
            "streaming index still has buffered connectors that do not implement callback streaming"
        );
    }

    // Set up progress tracking
    if let Some(p) = &opts.progress {
        p.phase.store(1, Ordering::Relaxed); // Scanning
        p.total.store(num_connectors, Ordering::Relaxed);
        p.current.store(0, Ordering::Relaxed);
        p.discovered_agents.store(0, Ordering::Relaxed);
        if let Ok(mut names) = p.discovered_agent_names.lock() {
            names.clear();
        }
    }

    // Create bounded channel for backpressure
    let (tx, rx) = bounded::<IndexMessage>(STREAMING_CHANNEL_SIZE);
    let producer_config = StreamingProducerConfig {
        flow_limiter: Arc::new(StreamingByteLimiter::new(STREAMING_MAX_BYTES_IN_FLIGHT)),
        data_dir: opts.data_dir.clone(),
        remote_roots: remote_roots.clone(),
        since_ts,
        progress: opts.progress.clone(),
    };

    // Spawn producer threads for each connector
    let handles: Vec<(&'static str, JoinHandle<()>)> = connector_factories
        .into_iter()
        .map(|(name, factory)| {
            (
                name,
                spawn_connector_producer(name, factory, tx.clone(), producer_config.clone()),
            )
        })
        .collect();

    // Drop our copy of the sender so channel closes when all producers finish
    drop(tx);

    // Run consumer on main thread
    let consumer_result = run_streaming_consumer(
        rx,
        num_connectors,
        storage,
        t_index,
        producer_config.flow_limiter.clone(),
        &opts.progress,
        needs_rebuild,
    );

    if consumer_result.is_err() {
        producer_config.flow_limiter.close();
    }

    let mut join_errors = Vec::new();
    for (name, handle) in handles {
        if let Err(payload) = handle.join() {
            let panic_message = panic_payload_message(payload);
            tracing::error!(connector = name, panic = %panic_message, "streaming producer panicked");
            join_errors.push(format!("{name}: {panic_message}"));
        }
    }

    if let Err(error) = consumer_result {
        if !join_errors.is_empty() {
            let combined = format!(
                "{error}; streaming producer thread panicked: {}",
                join_errors.join("; ")
            );
            set_progress_last_error(opts.progress.as_ref(), Some(combined.clone()));
            return Err(anyhow::anyhow!(combined));
        }
        set_progress_last_error(opts.progress.as_ref(), Some(error.to_string()));
        return Err(error);
    }

    if !join_errors.is_empty() {
        let error = format!(
            "streaming producer thread panicked: {}",
            join_errors.join("; ")
        );
        set_progress_last_error(opts.progress.as_ref(), Some(error.clone()));
        return Err(anyhow::anyhow!(error));
    }

    let discovered_names = match consumer_result {
        Ok(names) => names,
        Err(_) => unreachable!("handled above"),
    };

    // Update discovered agent names in progress tracker
    if let Some(p) = &opts.progress
        && let Ok(mut names) = p.discovered_agent_names.lock()
    {
        names.extend(discovered_names);
    }

    Ok(())
}

/// Run indexing using original batch collection architecture.
///
/// This uses rayon's par_iter to scan all connectors in parallel, collecting
/// all conversations into memory before ingesting. This is the fallback when
/// streaming is disabled via CASS_STREAMING_INDEX=0.
fn run_batch_index(
    storage: &FrankenStorage,
    t_index: &mut TantivyIndex,
    opts: &IndexOptions,
    since_ts: Option<i64>,
    needs_rebuild: bool,
    remote_roots: Vec<ScanRoot>,
) -> Result<()> {
    let connector_factories = get_connector_factories();

    // First pass: Scan all to get counts if we have progress tracker
    // Use parallel iteration for faster agent discovery
    if let Some(p) = &opts.progress {
        p.phase.store(1, Ordering::Relaxed); // Scanning
        // Track connector scan progress during discovery.
        p.total.store(connector_factories.len(), Ordering::Relaxed);
        p.current.store(0, Ordering::Relaxed);
        p.discovered_agents.store(0, Ordering::Relaxed);
        if let Ok(mut names) = p.discovered_agent_names.lock() {
            names.clear();
        }
    }

    // Run connector detection and scanning in parallel using rayon
    // Optimization 2.2: Eliminate mutex lock contention on discovered_agent_names
    // by collecting names after the parallel phase instead of locking inside par_iter.
    use rayon::prelude::*;

    let progress_ref = opts.progress.as_ref();
    let data_dir = opts.data_dir.clone();

    // Return type includes whether agent was discovered (for post-parallel name collection)
    let pending_batches: Vec<(&'static str, Vec<NormalizedConversation>, bool)> =
        connector_factories
            .into_par_iter()
            .filter_map(|(name, factory)| {
                let conn = factory();
                let detect = conn.detect();
                let was_detected = detect.detected;
                let mut convs = Vec::new();
                let mut is_discovered = false;

                if detect.detected {
                    // Update discovered agents count immediately when detected
                    // This gives fast UI feedback during the discovery phase
                    // Note: AtomicUsize has no contention, only the mutex was problematic
                    if let Some(p) = progress_ref {
                        p.discovered_agents.fetch_add(1, Ordering::Relaxed);
                    }
                    is_discovered = true;

                    let ctx =
                        crate::connectors::ScanContext::local_default(data_dir.clone(), since_ts);
                    match conn.scan(&ctx) {
                        Ok(mut local_convs) => {
                            let local_origin = Origin::local();
                            for conv in &mut local_convs {
                                inject_provenance(conv, &local_origin);
                            }
                            convs.extend(local_convs);
                        }
                        Err(e) => {
                            // Note: agent was counted as discovered but scan failed
                            // This is acceptable as detection succeeded (agent exists)
                            tracing::warn!("scan failed for {}: {}", name, e);
                        }
                    }
                }

                if !remote_roots.is_empty() {
                    for root in &remote_roots {
                        let ctx = crate::connectors::ScanContext::with_roots(
                            root.path.clone(),
                            vec![root.clone()],
                            since_ts,
                        );
                        match conn.scan(&ctx) {
                            Ok(mut remote_convs) => {
                                for conv in &mut remote_convs {
                                    inject_provenance(conv, &root.origin);
                                    apply_workspace_rewrite(conv, root);
                                }
                                convs.extend(remote_convs);
                            }
                            Err(e) => {
                                tracing::warn!(
                                    connector = name,
                                    root = %root.path.display(),
                                    "remote scan failed: {e}"
                                );
                            }
                        }
                    }
                }

                // Agent discovered via remote scan (wasn't detected locally but has conversations)
                if !was_detected && !convs.is_empty() {
                    if let Some(p) = progress_ref {
                        p.discovered_agents.fetch_add(1, Ordering::Relaxed);
                    }
                    is_discovered = true;
                }

                // Mark this connector as scanned for discovery progress.
                if let Some(p) = progress_ref {
                    p.current.fetch_add(1, Ordering::Relaxed);
                }

                if convs.is_empty() && !is_discovered {
                    return None;
                }

                tracing::info!(
                    connector = name,
                    conversations = convs.len(),
                    discovered = is_discovered,
                    "batch_scan_complete"
                );
                Some((name, convs, is_discovered))
            })
            .collect();

    // Post-parallel phase: collect discovered agent names with single mutex lock
    // This eliminates O(connectors) mutex acquisitions during parallel execution
    if let Some(p) = &opts.progress {
        let discovered_names: Vec<&str> = pending_batches
            .iter()
            .filter(|(_, _, discovered)| *discovered)
            .map(|(name, _, _)| *name)
            .collect();

        if let Ok(mut names) = p.discovered_agent_names.lock() {
            names.extend(discovered_names.into_iter().map(String::from));
        }

        let total_conversations: usize = pending_batches
            .iter()
            .map(|(_, convs, _)| convs.len())
            .sum();
        p.phase.store(2, Ordering::Relaxed); // Indexing
        p.total.store(total_conversations, Ordering::Relaxed);
        p.current.store(0, Ordering::Relaxed);
    }

    for (name, convs, _discovered) in pending_batches {
        ingest_batch(storage, t_index, &convs, &opts.progress, needs_rebuild)?;
        tracing::info!(
            connector = name,
            conversations = convs.len(),
            "batch_ingest"
        );
    }

    Ok(())
}

pub fn run_index(
    opts: IndexOptions,
    event_channel: Option<(Sender<IndexerEvent>, Receiver<IndexerEvent>)>,
) -> Result<()> {
    let _progress_reset = RunIndexProgressReset::new(opts.progress.clone());
    set_progress_last_error(opts.progress.as_ref(), None);
    let _index_run_lock = acquire_index_run_lock(&opts.data_dir, &opts.db_path)?;

    let (storage, storage_rebuilt, opened_fresh_for_full) =
        open_storage_for_index(&opts.db_path, opts.full)?;
    let mut storage = storage;
    let mut storage_rebuilt = storage_rebuilt;
    let mut opened_fresh_for_full = opened_fresh_for_full;
    persist::apply_index_writer_busy_timeout(&storage);
    let index_path = index_dir(&opts.data_dir)?;
    let mut initial_canonical_sessions_before_salvage = count_total_conversations_exact(&storage)?;
    if opts.full
        && !opened_fresh_for_full
        && full_rebuild_requires_historical_restart(
            &storage,
            &opts.db_path,
            initial_canonical_sessions_before_salvage,
        )?
    {
        tracing::info!(
            db_path = %opts.db_path.display(),
            conversations = initial_canonical_sessions_before_salvage,
            "full rebuild detected incomplete historical salvage state; restarting from a fresh canonical database"
        );
        storage = reopen_fresh_storage_for_full_rebuild(storage, &opts.db_path)?;
        storage_rebuilt = true;
        opened_fresh_for_full = true;
        persist::apply_index_writer_busy_timeout(&storage);
        initial_canonical_sessions_before_salvage = count_total_conversations_exact(&storage)?;
    }
    let canonical_only_full_rebuild = opts.full
        && initial_canonical_sessions_before_salvage > 0
        && !storage_rebuilt
        && !opened_fresh_for_full;
    let resume_lexical_rebuild = if initial_canonical_sessions_before_salvage > 0 {
        let db_state = lexical_rebuild_db_state(&storage, &opts.db_path)?;
        has_pending_lexical_rebuild(&index_path, &db_state)?
    } else {
        false
    };
    if opts.full && !resume_lexical_rebuild {
        clear_lexical_rebuild_state(&index_path)?;
    }
    if opts.full && !canonical_only_full_rebuild {
        repair_daily_stats_if_drifted(&storage, &opts.db_path)?;
    } else if canonical_only_full_rebuild {
        tracing::info!(
            db_path = %opts.db_path.display(),
            conversations = initial_canonical_sessions_before_salvage,
            "deferring daily_stats repair because full rebuild is reindexing an already-populated canonical database"
        );
    }
    let mut performed_scan = false;

    // Detect if we are rebuilding due to missing meta/schema mismatch/index corruption.
    // IMPORTANT: This must stay aligned with TantivyIndex::open_or_create() rebuild triggers.
    let schema_hash_path = index_path.join("schema_hash.json");
    let schema_matches = schema_hash_path.exists()
        && std::fs::read_to_string(&schema_hash_path)
            .ok()
            .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
            .and_then(|json| {
                json.get("schema_hash")
                    .and_then(|v| v.as_str())
                    .map(schema_hash_matches)
            })
            .unwrap_or(false);

    // Treat missing schema hash as rebuild (open_or_create will wipe/recreate).
    let mut needs_rebuild = storage_rebuilt
        || opts.force_rebuild
        || !index_path.join("meta.json").exists()
        || !schema_matches;

    // Preflight open: if the cass-compatible Tantivy reader can't open, force a
    // rebuild so we do a full scan and reindex messages into the new index
    // (SQLite is incremental-only by default).
    if !needs_rebuild
        && let Err(e) = frankensearch::lexical::cass_open_search_reader(
            &index_path,
            frankensearch::lexical::ReloadPolicy::Manual,
        )
    {
        tracing::warn!(
            error = %e,
            path = %index_path.display(),
            "tantivy open preflight failed; forcing rebuild"
        );
        needs_rebuild = true;
    }

    if needs_rebuild && let Some(p) = &opts.progress {
        p.is_rebuilding.store(true, Ordering::Relaxed);
    }

    if needs_rebuild && !resume_lexical_rebuild {
        // Clean slate: avoid stale lock files and ensure a fresh Tantivy index.
        let _ = std::fs::remove_dir_all(&index_path);
    }
    // Record scan start time before scanning
    let scan_start_ts = FrankenStorage::now_millis();

    // Keep sources table in sync with sources.toml for provenance integrity.
    sync_sources_config_to_db(&storage);

    let scan_roots = build_scan_roots(&storage, &opts.data_dir);
    let remote_roots: Vec<ScanRoot> = scan_roots
        .iter()
        .filter(|r| r.origin.is_remote())
        .cloned()
        .collect();

    let t_index = if resume_lexical_rebuild {
        tracing::info!(
            db_path = %opts.db_path.display(),
            "resuming incomplete lexical rebuild from canonical database checkpoint"
        );
        rebuild_tantivy_from_db(
            &opts.db_path,
            &opts.data_dir,
            initial_canonical_sessions_before_salvage,
            opts.progress.clone(),
        )?;
        TantivyIndex::open_or_create(&index_path)?
    } else {
        let mut t_index = TantivyIndex::open_or_create(&index_path)?;

        if opts.full && !opened_fresh_for_full && initial_canonical_sessions_before_salvage == 0 {
            storage = reopen_fresh_storage_for_full_rebuild(storage, &opts.db_path)?;
            persist::apply_index_writer_busy_timeout(&storage);
            t_index.delete_all()?;
            t_index.commit()?;
        } else if opts.full {
            t_index.delete_all()?;
            t_index.commit()?;
        }

        let canonical_sessions_before_salvage = count_total_conversations_exact(&storage)?;
        let mut has_pending_historical_bundles =
            storage.has_pending_historical_bundles(&opts.db_path)?;
        let canonical_only_full_rebuild = opts.full
            && canonical_sessions_before_salvage > 0
            && !storage_rebuilt
            && !opened_fresh_for_full;
        let should_salvage_historical = should_salvage_historical_databases(
            storage_rebuilt,
            canonical_sessions_before_salvage,
            has_pending_historical_bundles,
            canonical_only_full_rebuild,
        );
        tracing::warn!(
            db_path = %opts.db_path.display(),
            storage_rebuilt,
            opened_fresh_for_full,
            canonical_sessions_before_salvage,
            has_pending_historical_bundles,
            canonical_only_full_rebuild,
            should_salvage_historical,
            "historical salvage decision"
        );
        let historical_salvage: HistoricalSalvageOutcome = if should_salvage_historical {
            let mut outcome = HistoricalSalvageOutcome::default();
            if canonical_sessions_before_salvage == 0 {
                let (reopened_storage, seed_outcome) =
                    maybe_seed_empty_canonical_from_historical_bundle(storage, &opts.db_path)?;
                storage = reopened_storage;
                persist::apply_index_writer_busy_timeout(&storage);
                if let Some(seed_outcome) = seed_outcome {
                    outcome.accumulate(seed_outcome);
                    has_pending_historical_bundles =
                        storage.has_pending_historical_bundles(&opts.db_path)?;
                }
            }
            if has_pending_historical_bundles {
                outcome.accumulate(storage.salvage_historical_databases(&opts.db_path)?);
            } else {
                tracing::info!(
                    db_path = %opts.db_path.display(),
                    "skipping incremental historical salvage because all discoverable historical bundles are already recorded in the canonical database"
                );
            }
            outcome
        } else {
            tracing::info!(
                db_path = %opts.db_path.display(),
                conversations = canonical_sessions_before_salvage,
                pending_historical_bundles = has_pending_historical_bundles,
                "skipping historical salvage because canonical database is already populated and no additional historical bundles are pending"
            );
            HistoricalSalvageOutcome::default()
        };
        if historical_salvage.messages_imported > 0 {
            tracing::info!(
                bundles_imported = historical_salvage.bundles_imported,
                conversations_imported = historical_salvage.conversations_imported,
                messages_imported = historical_salvage.messages_imported,
                "historical cass bundles merged into canonical database before scan"
            );
        }
        let rebuild_from_canonical_only =
            canonical_only_full_rebuild && historical_salvage.conversations_imported == 0;

        if historical_salvage.conversations_imported > 0
            || (opts.full && !rebuild_from_canonical_only)
        {
            repair_daily_stats_if_drifted(&storage, &opts.db_path)?;
        }

        if rebuild_from_canonical_only {
            tracing::info!(
                db_path = %opts.db_path.display(),
                conversations = initial_canonical_sessions_before_salvage,
                "skipping raw source rescan during full rebuild because the canonical database is already populated"
            );
        }

        if rebuild_from_canonical_only {
            drop(t_index);
            rebuild_tantivy_from_db(
                &opts.db_path,
                &opts.data_dir,
                count_total_conversations_exact(&storage)?,
                opts.progress.clone(),
            )?;
            t_index = TantivyIndex::open_or_create(&index_path)?;
        } else {
            // Get last scan timestamp for incremental indexing.
            // If full rebuild or force_rebuild, scan everything (since_ts = None).
            // Otherwise, only scan files modified since last successful scan.
            let since_ts = if opts.full || needs_rebuild {
                None
            } else {
                storage
                    .get_last_scan_ts()
                    .unwrap_or(None)
                    .map(|ts| ts.saturating_sub(1))
            };

            if since_ts.is_some() {
                tracing::info!(since_ts = ?since_ts, "incremental_scan: using last_scan_ts");
            } else {
                tracing::info!("full_scan: no last_scan_ts or rebuild requested");
            }

            // Choose between streaming indexing (Opt 8.2) and batch indexing
            if streaming_index_enabled() {
                tracing::info!("using streaming indexing (Opt 8.2)");
                run_streaming_index(
                    &storage,
                    &mut t_index,
                    &opts,
                    since_ts,
                    needs_rebuild,
                    remote_roots.clone(),
                )?;
            } else {
                tracing::info!(
                    "using batch indexing (streaming disabled via CASS_STREAMING_INDEX=0)"
                );
                run_batch_index(
                    &storage,
                    &mut t_index,
                    &opts,
                    since_ts,
                    needs_rebuild,
                    remote_roots.clone(),
                )?;
            }
            performed_scan = true;

            t_index.commit()?;

            if opts.full || historical_salvage.messages_imported > 0 {
                drop(t_index);
                rebuild_tantivy_from_db(
                    &opts.db_path,
                    &opts.data_dir,
                    count_total_conversations_exact(&storage)?,
                    opts.progress.clone(),
                )?;
                t_index = TantivyIndex::open_or_create(&index_path)?;
            }
        }

        t_index
    };

    // Semantic indexing (if enabled)
    if opts.semantic {
        // In watch mode, skip the expensive bulk re-embed if a vector index and
        // watermark already exist. The incremental path in the watch callback
        // will pick up any new messages via WAL append.
        let vi_dir = opts
            .data_dir
            .join(crate::search::vector_index::VECTOR_INDEX_DIR);
        let has_existing_index = vi_dir.is_dir()
            && std::fs::read_dir(&vi_dir)
                .map(|entries| {
                    entries
                        .filter_map(|e| e.ok())
                        .any(|e| e.path().extension().is_some_and(|ext| ext == "fsvi"))
                })
                .unwrap_or(false);
        let has_watermark = storage.get_last_embedded_message_id()?.is_some();

        if opts.watch && has_existing_index && has_watermark {
            tracing::info!(
                dir = %vi_dir.display(),
                "skipping bulk semantic re-embed (existing index + watermark found); \
                 incremental watch callback will handle new messages"
            );
        } else {
            tracing::info!(embedder = %opts.embedder, "starting semantic indexing");

            let semantic_indexer = SemanticIndexer::new(&opts.embedder, Some(&opts.data_dir))?;

            // Fetch all messages with metadata from SQLite
            let raw_messages = storage.fetch_messages_for_embedding()?;
            tracing::info!(
                message_count = raw_messages.len(),
                "fetched messages for embedding"
            );

            // Convert to EmbeddingInput format
            let embedding_inputs: Vec<EmbeddingInput> = raw_messages
                .into_iter()
                .filter_map(|msg| {
                    let role_u8 = match msg.role.as_str() {
                        "user" => ROLE_USER,
                        "agent" | "assistant" => ROLE_ASSISTANT,
                        "system" => ROLE_SYSTEM,
                        "tool" => ROLE_TOOL,
                        _ => ROLE_USER, // default to user for unknown roles
                    };

                    let Some(message_id) = message_id_from_db(msg.message_id) else {
                        tracing::warn!(
                            raw_message_id = msg.message_id,
                            "Skipping message with out-of-range id during semantic indexing"
                        );
                        return None;
                    };

                    Some(EmbeddingInput {
                        message_id,
                        created_at_ms: msg.created_at.unwrap_or(0),
                        agent_id: saturating_u32_from_i64(msg.agent_id),
                        workspace_id: saturating_u32_from_i64(msg.workspace_id.unwrap_or(0)),
                        source_id: msg.source_id_hash,
                        role: role_u8,
                        chunk_idx: 0,
                        content: msg.content,
                    })
                })
                .collect();

            // Generate embeddings
            let embedded_messages = semantic_indexer.embed_messages(&embedding_inputs)?;
            tracing::info!(
                embedded_count = embedded_messages.len(),
                "generated embeddings"
            );

            if !embedded_messages.is_empty() {
                let vector_index =
                    semantic_indexer.build_and_save_index(embedded_messages, &opts.data_dir)?;
                let index_path = crate::search::vector_index::vector_index_path(
                    &opts.data_dir,
                    semantic_indexer.embedder_id(),
                );
                tracing::info!(
                    path = %index_path.display(),
                    embedder = semantic_indexer.embedder_id(),
                    "saved semantic vector index"
                );

                // Build HNSW index for approximate nearest neighbor search (if enabled)
                if opts.build_hnsw {
                    let hnsw_path = semantic_indexer.build_hnsw_index(
                        &vector_index,
                        &opts.data_dir,
                        None, // Use default M
                        None, // Use default ef_construction
                    )?;
                    tracing::info!(
                        path = %hnsw_path.display(),
                        embedder = semantic_indexer.embedder_id(),
                        "saved HNSW index for approximate search"
                    );
                }
            }

            // Set watermark so incremental watch-mode embedding only sees new messages
            if let Some(max_id) = embedding_inputs.iter().map(|e| e.message_id).max() {
                storage.set_last_embedded_message_id(i64::try_from(max_id).unwrap_or(i64::MAX))?;
            }
        }
    }

    // Update last_scan_ts after successful scan and commit. Pure lexical-resume
    // runs intentionally preserve the previous scan watermark.
    if performed_scan {
        persist::with_concurrent_retry(persist::begin_concurrent_retry_limit(), || {
            storage.set_last_scan_ts(scan_start_ts)
        })
        .with_context(|| {
            format!(
                "updating last_scan_ts after index run for {}",
                opts.db_path.display()
            )
        })?;
        tracing::info!(
            scan_start_ts,
            "updated last_scan_ts for incremental indexing"
        );
    } else {
        tracing::info!(
            db_path = %opts.db_path.display(),
            "preserving last_scan_ts because this run only resumed the lexical rebuild"
        );
    }

    // Update last_indexed_at so `cass status` reflects the latest index time
    let now_ms = FrankenStorage::now_millis();
    persist::with_concurrent_retry(persist::begin_concurrent_retry_limit(), || {
        storage.set_last_indexed_at(now_ms)
    })
    .with_context(|| {
        format!(
            "updating last_indexed_at after index run for {}",
            opts.db_path.display()
        )
    })?;
    tracing::info!(now_ms, "updated last_indexed_at for status display");

    let mut storage = Some(storage);

    if opts.full {
        // Close the live frankensqlite handle before rusqlite mutates the FTS
        // virtual-table schema. Keeping both handles open across the rebuild can
        // leave stale schema state in memory and corrupt the shadow-table rootpages.
        close_storage_after_index(
            storage
                .take()
                .expect("storage handle should exist before FTS repair"),
            &opts.db_path,
            "pre-FTS compatibility repair",
        )?;
        let repair = crate::storage::sqlite::ensure_fts_consistency_via_rusqlite(&opts.db_path)
            .with_context(|| {
                format!(
                    "repairing stock-compatible FTS table after full index run for {}",
                    opts.db_path.display()
                )
            })?;
        match repair {
            crate::storage::sqlite::FtsConsistencyRepair::AlreadyHealthy { rows } => {
                tracing::info!(
                    db_path = %opts.db_path.display(),
                    rows,
                    "stock-compatible FTS table already matched canonical messages after full index run"
                );
            }
            crate::storage::sqlite::FtsConsistencyRepair::IncrementalCatchUp {
                inserted_rows,
                total_rows,
            } => {
                tracing::info!(
                    db_path = %opts.db_path.display(),
                    inserted_rows,
                    total_rows,
                    "incrementally repaired missing stock-compatible FTS rows after full index run"
                );
            }
            crate::storage::sqlite::FtsConsistencyRepair::Rebuilt { inserted_rows } => {
                tracing::info!(
                    db_path = %opts.db_path.display(),
                    inserted_rows,
                    "fully rebuilt stock-compatible FTS table after full index run"
                );
            }
        }

        if opts.watch || opts.watch_once_paths.is_some() {
            storage = Some(FrankenStorage::open(&opts.db_path).with_context(|| {
                format!(
                    "reopening canonical db after FTS compatibility repair: {}",
                    opts.db_path.display()
                )
            })?);
        }
    }

    reset_progress_to_idle(opts.progress.as_ref());

    if opts.watch || opts.watch_once_paths.is_some() {
        let opts_clone = opts.clone();
        let state = Mutex::new(load_watch_state(&opts.data_dir));
        let storage = Arc::new(Mutex::new(
            storage
                .take()
                .expect("watch mode requires a live storage handle"),
        ));
        let storage_for_watch = Arc::clone(&storage);
        let t_index = Mutex::new(t_index);

        // Semantic embedding cooldown state for watch mode.
        // The initial pass already embedded everything, so we start the clock
        // from now — the cooldown must elapse before the first incremental pass.
        let semantic_enabled = opts.semantic;
        let embedder_id = opts.embedder.clone();
        let data_dir_for_semantic = opts.data_dir.clone();
        let semantic_cooldown = Duration::from_secs(
            dotenvy::var("CASS_WATCH_SEMANTIC_COOLDOWN_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(60),
        );
        let last_semantic_embed = Mutex::new(Instant::now());

        // Initialize stale detector for watch mode
        let stale_detector = Arc::new(StaleDetector::from_env());
        let stale_config = StaleConfig::from_env();
        if stale_config.is_enabled() {
            tracing::info!(
                action = ?stale_config.action,
                threshold_hours = stale_config.threshold_hours,
                check_interval_mins = stale_config.check_interval_mins,
                "stale detection enabled"
            );
        }

        // Detect roots once for the watcher setup
        // Includes both local detected roots and all remote mirror roots
        let watch_roots = build_watch_roots(remote_roots.clone());

        // Clone detector for the callback
        let detector_clone = stale_detector.clone();

        let watch_result = watch_sources(
            opts.watch_once_paths.clone(),
            watch_roots.clone(),
            event_channel,
            stale_detector,
            opts.watch_interval_secs,
            move |paths, roots, is_rebuild| {
                let indexed = if is_rebuild {
                    if let Ok(mut g) = state.lock() {
                        g.clear();
                        if let Err(e) = save_watch_state(&opts_clone.data_dir, &g) {
                            tracing::warn!("failed to save watch state: {e}");
                        }
                    }
                    // Reset stale detector on rebuild
                    detector_clone.reset();
                    // For rebuild, trigger reindex on all active roots
                    let all_root_paths: Vec<PathBuf> =
                        roots.iter().map(|(_, root)| root.path.clone()).collect();
                    let indexed = reindex_paths(
                        &opts_clone,
                        all_root_paths,
                        roots,
                        &state,
                        &storage_for_watch,
                        &t_index,
                        true,
                    );
                    finalize_watch_reindex_result(
                        indexed,
                        &detector_clone,
                        opts_clone.progress.as_ref(),
                        "watch rebuild reindex",
                    )
                } else {
                    let indexed = finalize_watch_reindex_result(
                        reindex_paths(
                            &opts_clone,
                            paths,
                            roots,
                            &state,
                            &storage_for_watch,
                            &t_index,
                            false,
                        ),
                        &detector_clone,
                        opts_clone.progress.as_ref(),
                        "watch incremental reindex",
                    );

                    // Merge Tantivy segments if idle conditions are met.
                    // Without this, each reindex_paths() commit creates a new
                    // segment, leading to unbounded accumulation over weeks of
                    // continuous watch mode operation. The cooldown logic inside
                    // optimize_if_idle() (300s, 4-segment threshold) prevents
                    // over-merging. See issue #87.
                    if let Ok(mut guard) = t_index.lock()
                        && let Err(e) = guard.optimize_if_idle()
                    {
                        tracing::warn!(error = %e, "segment merge failed during watch");
                    }
                    indexed
                };

                // Incremental semantic embedding with cooldown
                if semantic_enabled && indexed > 0 {
                    let should_embed = last_semantic_embed
                        .lock()
                        .map(|t| t.elapsed() >= semantic_cooldown)
                        .unwrap_or(false);
                    if should_embed {
                        match incremental_semantic_embed(
                            &embedder_id,
                            &data_dir_for_semantic,
                            &storage_for_watch,
                        ) {
                            Ok(0) => {} // no new messages to embed
                            Ok(n) => {
                                tracing::info!(
                                    count = n,
                                    "incremental semantic embedding complete"
                                );
                                if let Ok(mut t) = last_semantic_embed.lock() {
                                    *t = Instant::now();
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "incremental semantic embedding failed");
                                // Reset cooldown on error to avoid rapid-fire retries
                                if let Ok(mut t) = last_semantic_embed.lock() {
                                    *t = Instant::now();
                                }
                            }
                        }
                    }
                }
            },
        );

        let close_result =
            release_watch_storage_after_index(storage, &opts.db_path, "watch indexing session");
        if let Err(err) = watch_result {
            if let Err(close_err) = close_result {
                tracing::warn!(
                    error = %close_err,
                    db_path = %opts.db_path.display(),
                    "failed to close canonical db cleanly after watch indexing error"
                );
            }
            return Err(err);
        }
        close_result?;
        return Ok(());
    }

    if let Some(storage) = storage {
        close_storage_after_index(storage, &opts.db_path, "index run")
    } else {
        Ok(())
    }
}

fn close_storage_after_index(storage: FrankenStorage, db_path: &Path, context: &str) -> Result<()> {
    storage.close().with_context(|| {
        format!(
            "closing canonical db after {context}: {}",
            db_path.display()
        )
    })
}

fn release_watch_storage_after_index(
    storage: Arc<Mutex<FrankenStorage>>,
    db_path: &Path,
    context: &str,
) -> Result<()> {
    let storage = Arc::try_unwrap(storage).map_err(|_| {
        anyhow::anyhow!(
            "watch indexing retained extra canonical db handles while closing {}",
            db_path.display()
        )
    })?;
    match storage.into_inner() {
        Ok(storage) => close_storage_after_index(storage, db_path, context),
        Err(poisoned) => {
            let mut storage = poisoned.into_inner();
            storage.close_best_effort_in_place();
            Err(anyhow::anyhow!(
                "storage mutex poisoned while closing canonical db after {context}: {}",
                db_path.display()
            ))
        }
    }
}

/// Perform incremental semantic embedding for messages added since the last
/// watermark. Loads the ONNX model, embeds the batch, appends to the existing
/// FSVI index via WAL, and updates the watermark.
fn incremental_semantic_embed(
    embedder: &str,
    data_dir: &Path,
    storage: &Mutex<FrankenStorage>,
) -> Result<usize> {
    // 1. Read watermark
    let watermark = storage
        .lock()
        .map_err(|e| anyhow::anyhow!("lock storage for watermark read: {e}"))?
        .get_last_embedded_message_id()?
        .unwrap_or(0);

    // 2. Fetch new messages since watermark
    let raw_messages = storage
        .lock()
        .map_err(|e| anyhow::anyhow!("lock storage for message fetch: {e}"))?
        .fetch_messages_for_embedding_since(watermark)?;

    if raw_messages.is_empty() {
        return Ok(0);
    }

    // Track the max raw DB id so we always advance the watermark, even if
    // all messages are filtered out (e.g., empty content, out-of-range ids).
    let raw_max_id = raw_messages.iter().map(|m| m.message_id).max().unwrap_or(0);

    tracing::info!(
        since_id = watermark,
        count = raw_messages.len(),
        "incremental semantic: fetched new messages"
    );

    // 3. Convert to EmbeddingInput
    let embedding_inputs: Vec<EmbeddingInput> = raw_messages
        .into_iter()
        .filter_map(|msg| {
            let role_u8 = match msg.role.as_str() {
                "user" => ROLE_USER,
                "agent" | "assistant" => ROLE_ASSISTANT,
                "system" => ROLE_SYSTEM,
                "tool" => ROLE_TOOL,
                _ => ROLE_USER,
            };

            let Some(message_id) = message_id_from_db(msg.message_id) else {
                tracing::warn!(
                    raw_message_id = msg.message_id,
                    "skipping out-of-range id during incremental semantic indexing"
                );
                return None;
            };

            Some(EmbeddingInput {
                message_id,
                created_at_ms: msg.created_at.unwrap_or(0),
                agent_id: saturating_u32_from_i64(msg.agent_id),
                workspace_id: saturating_u32_from_i64(msg.workspace_id.unwrap_or(0)),
                source_id: msg.source_id_hash,
                role: role_u8,
                chunk_idx: 0,
                content: msg.content,
            })
        })
        .collect();

    if embedding_inputs.is_empty() {
        // All messages were filtered out; advance watermark to avoid re-fetching
        storage
            .lock()
            .map_err(|e| anyhow::anyhow!("lock storage for watermark write: {e}"))?
            .set_last_embedded_message_id(raw_max_id)?;
        return Ok(0);
    }

    // 4. Load model, embed, append to existing index
    let semantic_indexer = SemanticIndexer::new(embedder, Some(data_dir))?;

    let embedded = semantic_indexer.embed_messages(&embedding_inputs)?;
    let count = semantic_indexer.append_to_index(embedded, data_dir)?;

    // 5. Update watermark to highest raw DB id (not filtered embedding id)
    storage
        .lock()
        .map_err(|e| anyhow::anyhow!("lock storage for watermark write: {e}"))?
        .set_last_embedded_message_id(raw_max_id)?;

    Ok(count)
}

/// Open frankensqlite storage for indexing with forward-compatibility recovery.
///
/// Returns `(storage, rebuilt)` where `rebuilt=true` means we detected an
/// incompatible/future schema, backed up + recreated the DB, and reopened it.
fn open_storage_for_index(
    db_path: &Path,
    allow_full_recovery: bool,
) -> Result<(FrankenStorage, bool, bool)> {
    match FrankenStorage::open_or_rebuild(db_path) {
        Ok(storage) => Ok((storage, false, false)),
        Err(MigrationError::RebuildRequired {
            reason,
            backup_path,
        }) => {
            tracing::warn!(
                db_path = %db_path.display(),
                reason = %reason,
                backup_path = ?backup_path.as_ref().map(|p| p.display().to_string()),
                "storage schema incompatible; rebuilt database before indexing"
            );
            let storage = FrankenStorage::open(db_path)?;
            Ok((storage, true, true))
        }
        Err(err) if allow_full_recovery => {
            tracing::warn!(
                db_path = %db_path.display(),
                error = %err,
                "full rebuild storage open failed; backing up and reopening with a fresh canonical db"
            );
            let backup_path =
                crate::storage::sqlite::create_backup(db_path).map_err(|backup_err| {
                    anyhow::anyhow!(
                        "backing up busy/corrupt canonical db before full rebuild: {backup_err}"
                    )
                })?;
            if db_path.exists() {
                crate::storage::sqlite::remove_database_files(db_path).with_context(|| {
                    format!(
                        "removing busy/corrupt canonical db bundle before full rebuild: {}",
                        db_path.display()
                    )
                })?;
            }
            if let Some(path) = backup_path {
                tracing::info!(
                    db_path = %db_path.display(),
                    backup_path = %path.display(),
                    "backed up canonical db after full-rebuild open failure"
                );
            }
            let storage = FrankenStorage::open(db_path)?;
            Ok((storage, true, true))
        }
        Err(err) => Err(anyhow::anyhow!(
            "failed to open frankensqlite storage: {err}"
        )),
    }
}

fn reopen_fresh_storage_for_full_rebuild(
    storage: FrankenStorage,
    db_path: &Path,
) -> Result<FrankenStorage> {
    storage.close().with_context(|| {
        format!(
            "closing canonical db before replacing it for full rebuild: {}",
            db_path.display()
        )
    })?;

    let backup_path = crate::storage::sqlite::create_backup(db_path)
        .map_err(|err| anyhow::anyhow!("backing up canonical db before full rebuild: {err}"))?;
    if db_path.exists() {
        crate::storage::sqlite::remove_database_files(db_path).with_context(|| {
            format!(
                "removing existing canonical db bundle before full rebuild: {}",
                db_path.display()
            )
        })?;
    }

    if let Some(path) = backup_path {
        tracing::info!(
            db_path = %db_path.display(),
            backup_path = %path.display(),
            "replaced canonical db with a fresh empty database for full rebuild"
        );
    }

    FrankenStorage::open(db_path).with_context(|| {
        format!(
            "opening fresh canonical db for full rebuild: {}",
            db_path.display()
        )
    })
}

fn quarantine_failed_seed_bundle(db_path: &Path) -> Result<Option<PathBuf>> {
    if !db_path.exists() {
        return Ok(None);
    }

    let Some(parent) = db_path.parent() else {
        return Ok(None);
    };
    let db_name = db_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("agent_search.db");
    let backups_dir = parent.join("backups");
    fs::create_dir_all(&backups_dir).with_context(|| {
        format!(
            "creating backups directory for failed baseline seed bundle: {}",
            backups_dir.display()
        )
    })?;

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let backup_root = backups_dir.join(format!("{db_name}.{timestamp}.failed-baseline-seed.bak"));

    for suffix in ["", "-wal", "-shm"] {
        let src = if suffix.is_empty() {
            db_path.to_path_buf()
        } else {
            db_path.with_file_name(format!("{db_name}{suffix}"))
        };
        if !src.exists() {
            continue;
        }
        let dest = if suffix.is_empty() {
            backup_root.clone()
        } else {
            backup_root.with_file_name(format!(
                "{}{}",
                backup_root
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("agent_search.db.failed-baseline-seed.bak"),
                suffix
            ))
        };
        fs::rename(&src, &dest).with_context(|| {
            format!(
                "moving failed baseline seed bundle component {} -> {}",
                src.display(),
                dest.display()
            )
        })?;
    }

    Ok(Some(backup_root))
}

fn maybe_seed_empty_canonical_from_historical_bundle(
    storage: FrankenStorage,
    db_path: &Path,
) -> Result<(FrankenStorage, Option<HistoricalSalvageOutcome>)> {
    let conversation_count = count_total_conversations_exact(&storage)?;
    if conversation_count > 0 {
        return Ok((storage, None));
    }

    storage.close().with_context(|| {
        format!(
            "closing canonical db before baseline historical seed attempt: {}",
            db_path.display()
        )
    })?;
    match seed_canonical_from_best_historical_bundle(db_path) {
        Ok(result) => {
            let reopened = if result.is_some() {
                FrankenStorage::open_writer(db_path).with_context(|| {
                    format!(
                        "reopening canonical database after baseline historical seed attempt without rerunning migrations: {}",
                        db_path.display()
                    )
                })?
            } else {
                FrankenStorage::open(db_path).with_context(|| {
                    format!(
                        "reopening canonical database after baseline historical seed attempt: {}",
                        db_path.display()
                    )
                })?
            };
            Ok((reopened, result))
        }
        Err(err) => {
            tracing::warn!(
                db_path = %db_path.display(),
                error = %err,
                "baseline historical seed import failed; falling back to incremental salvage"
            );
            match FrankenStorage::open(db_path) {
                Ok(reopened) => Ok((reopened, None)),
                Err(reopen_err) => {
                    tracing::warn!(
                        db_path = %db_path.display(),
                        error = %reopen_err,
                        "canonical database could not be reopened after failed baseline seed; quarantining partial bundle"
                    );
                    let failed_seed_backup =
                        quarantine_failed_seed_bundle(db_path).with_context(|| {
                            format!(
                                "quarantining failed baseline seed bundle before incremental salvage: {}",
                                db_path.display()
                            )
                        })?;
                    if let Some(path) = failed_seed_backup {
                        tracing::info!(
                            db_path = %db_path.display(),
                            backup_path = %path.display(),
                            "moved failed baseline seed bundle aside before incremental salvage fallback"
                        );
                    }
                    let reopened = FrankenStorage::open(db_path).with_context(|| {
                        format!(
                            "recreating fresh canonical database after failed baseline seed import: {}",
                            db_path.display()
                        )
                    })?;
                    Ok((reopened, None))
                }
            }
        }
    }
}

pub(crate) fn rebuild_tantivy_from_db(
    db_path: &Path,
    data_dir: &Path,
    total_conversations: usize,
    progress: Option<Arc<IndexingProgress>>,
) -> Result<usize> {
    use crate::model::types::MessageRole;

    let storage = FrankenStorage::open_readonly(db_path).with_context(|| {
        format!(
            "opening database for Tantivy rebuild: {}",
            db_path.display()
        )
    })?;

    let sources = storage.list_sources().unwrap_or_default();
    let mut source_map: HashMap<String, (SourceKind, Option<String>)> = HashMap::new();
    for source in sources {
        source_map.insert(source.id, (source.kind, source.host_label));
    }

    let index_path = index_dir(data_dir)?;
    let db_state = lexical_rebuild_db_state(&storage, db_path)?;
    let mut rebuild_state = match load_lexical_rebuild_state(&index_path)? {
        Some(state) if state.matches_run(&db_state, LEXICAL_REBUILD_PAGE_SIZE) => {
            reconcile_pending_lexical_commit(&index_path, state)?
        }
        Some(state) => {
            tracing::info!(
                db_path = %db_path.display(),
                existing_db_path = %state.db.db_path,
                existing_total_conversations = state.db.total_conversations,
                existing_storage_fingerprint = %state.db.storage_fingerprint,
                "discarding incompatible lexical rebuild checkpoint and restarting from zero"
            );
            LexicalRebuildState::new(db_state.clone(), LEXICAL_REBUILD_PAGE_SIZE)
        }
        None => LexicalRebuildState::new(db_state.clone(), LEXICAL_REBUILD_PAGE_SIZE),
    };

    let restart_from_zero = rebuild_state.completed
        || (rebuild_state.committed_offset == 0 && rebuild_state.pending.is_none());
    if restart_from_zero {
        if let Err(err) = fs::remove_dir_all(&index_path)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            return Err(err)
                .with_context(|| format!("removing stale index {}", index_path.display()));
        }
        fs::create_dir_all(&index_path).with_context(|| {
            format!("creating rebuilt index directory {}", index_path.display())
        })?;
        rebuild_state = LexicalRebuildState::new(db_state, LEXICAL_REBUILD_PAGE_SIZE);
    }

    let mut t_index = match TantivyIndex::open_or_create(&index_path) {
        Ok(index) => index,
        Err(err) if rebuild_state.committed_offset > 0 || rebuild_state.pending.is_some() => {
            tracing::warn!(
                path = %index_path.display(),
                error = %err,
                "partial lexical index could not be reopened; restarting lexical rebuild from zero"
            );
            if let Err(remove_err) = fs::remove_dir_all(&index_path)
                && remove_err.kind() != std::io::ErrorKind::NotFound
            {
                return Err(remove_err).with_context(|| {
                    format!("removing unreadable index {}", index_path.display())
                });
            }
            fs::create_dir_all(&index_path).with_context(|| {
                format!(
                    "recreating lexical index directory after open failure {}",
                    index_path.display()
                )
            })?;
            rebuild_state = LexicalRebuildState::new(
                lexical_rebuild_db_state(&storage, db_path)?,
                LEXICAL_REBUILD_PAGE_SIZE,
            );
            TantivyIndex::open_or_create(&index_path)?
        }
        Err(err) => return Err(err),
    };

    if !lexical_rebuild_state_path(&index_path).exists() {
        persist_lexical_rebuild_state(&index_path, &rebuild_state)?;
    }

    if let Some(p) = &progress {
        p.phase.store(2, Ordering::Relaxed);
        p.is_rebuilding.store(true, Ordering::Relaxed);
        p.total.store(total_conversations, Ordering::Relaxed);
        p.current
            .store(rebuild_state.processed_conversations, Ordering::Relaxed);
        p.discovered_agents.store(0, Ordering::Relaxed);
    }

    if rebuild_state.completed
        || rebuild_state.committed_offset >= i64::try_from(total_conversations).unwrap_or(i64::MAX)
    {
        storage.close().with_context(|| {
            format!(
                "closing readonly database after confirming completed Tantivy rebuild: {}",
                db_path.display()
            )
        })?;
        if let Some(p) = &progress {
            p.phase.store(0, Ordering::Relaxed);
            p.is_rebuilding.store(false, Ordering::Relaxed);
        }
        return Ok(rebuild_state.indexed_docs);
    }

    let mut offset = rebuild_state.committed_offset;
    let page_size = LEXICAL_REBUILD_PAGE_SIZE;
    let commit_interval_conversations = lexical_rebuild_commit_interval_conversations();
    let commit_interval_messages = lexical_rebuild_commit_interval_messages();
    let commit_interval_message_bytes = lexical_rebuild_commit_interval_message_bytes();
    let mut indexed_docs = rebuild_state.indexed_docs;
    let mut processed_conversations = rebuild_state.processed_conversations;
    let mut conversations_since_commit = 0usize;
    let mut messages_since_commit = 0usize;
    let mut message_bytes_since_commit = 0usize;
    let commit_rebuild_progress = |offset: i64,
                                   processed_conversations: usize,
                                   indexed_docs: usize,
                                   rebuild_state: &mut LexicalRebuildState,
                                   t_index: &mut TantivyIndex|
     -> Result<()> {
        rebuild_state.record_pending_commit(
            offset,
            processed_conversations,
            indexed_docs,
            index_meta_fingerprint(&index_path)?,
        );
        persist_lexical_rebuild_state(&index_path, rebuild_state)?;
        t_index.commit()?;
        rebuild_state.finalize_commit(index_meta_fingerprint(&index_path)?);
        persist_lexical_rebuild_state(&index_path, rebuild_state)?;
        Ok(())
    };

    loop {
        let batch = storage.list_conversations_for_lexical_rebuild(page_size, offset)?;
        if batch.is_empty() {
            break;
        }

        for conv in batch {
            offset = offset.saturating_add(1);
            processed_conversations = processed_conversations.saturating_add(1);
            conversations_since_commit = conversations_since_commit.saturating_add(1);

            let Some(conv_id) = conv.id else {
                if let Some(p) = &progress {
                    p.current.fetch_add(1, Ordering::Relaxed);
                }
                continue;
            };
            // Querying each conversation separately avoids a pathological
            // frankensqlite execution path for the batched `IN (...) ORDER BY`
            // rebuild query that can consume enormous heap before the first
            // lexical checkpoint is committed. The per-conversation fetch is
            // forced onto the named messages(conversation_id, idx) index so the
            // rebuild does not devolve into a full table scan per conversation.
            let messages = storage.fetch_messages_for_lexical_rebuild(conv_id)?;

            let (kind, host_label) =
                source_map.get(&conv.source_id).cloned().unwrap_or_else(|| {
                    let fallback_kind = if conv.source_id == LOCAL_SOURCE_ID {
                        SourceKind::Local
                    } else {
                        SourceKind::Ssh
                    };
                    (fallback_kind, None)
                });
            let host = conv.origin_host.as_deref().or(host_label.as_deref());
            let mut metadata = serde_json::Value::Object(serde_json::Map::new());
            ensure_cass_origin(&mut metadata, &conv.source_id, kind, host);

            let mut conversation_message_count = 0usize;
            let mut conversation_message_bytes = 0usize;
            let normalized_messages: Vec<NormalizedMessage> = messages
                .into_iter()
                .map(|msg| {
                    conversation_message_count = conversation_message_count.saturating_add(1);
                    conversation_message_bytes =
                        conversation_message_bytes.saturating_add(msg.content.len());
                    let role = match msg.role {
                        MessageRole::User => "user".to_string(),
                        MessageRole::Agent => "assistant".to_string(),
                        MessageRole::Tool => "tool".to_string(),
                        MessageRole::System => "system".to_string(),
                        MessageRole::Other(other) => other,
                    };

                    NormalizedMessage {
                        idx: msg.idx,
                        role,
                        author: msg.author,
                        created_at: msg.created_at,
                        content: msg.content,
                        extra: msg.extra_json,
                        snippets: Vec::new(),
                    }
                })
                .collect();

            let normalized = NormalizedConversation {
                agent_slug: conv.agent_slug,
                external_id: conv.external_id,
                title: conv.title,
                workspace: conv.workspace,
                source_path: conv.source_path,
                started_at: conv.started_at,
                ended_at: conv.ended_at,
                metadata,
                messages: normalized_messages,
            };

            indexed_docs += normalized.messages.len();
            t_index.add_messages(&normalized, &normalized.messages)?;
            messages_since_commit =
                messages_since_commit.saturating_add(conversation_message_count);
            message_bytes_since_commit =
                message_bytes_since_commit.saturating_add(conversation_message_bytes);

            if let Some(p) = &progress {
                p.current.fetch_add(1, Ordering::Relaxed);
            }

            if should_commit_lexical_rebuild(
                conversations_since_commit,
                messages_since_commit,
                message_bytes_since_commit,
                commit_interval_conversations,
                commit_interval_messages,
                commit_interval_message_bytes,
            ) {
                commit_rebuild_progress(
                    offset,
                    processed_conversations,
                    indexed_docs,
                    &mut rebuild_state,
                    &mut t_index,
                )?;
                conversations_since_commit = 0;
                messages_since_commit = 0;
                message_bytes_since_commit = 0;
            }
        }
        if should_commit_lexical_rebuild(
            conversations_since_commit,
            messages_since_commit,
            message_bytes_since_commit,
            commit_interval_conversations,
            commit_interval_messages,
            commit_interval_message_bytes,
        ) {
            commit_rebuild_progress(
                offset,
                processed_conversations,
                indexed_docs,
                &mut rebuild_state,
                &mut t_index,
            )?;
            conversations_since_commit = 0;
            messages_since_commit = 0;
            message_bytes_since_commit = 0;
        }
    }

    if conversations_since_commit > 0
        || messages_since_commit > 0
        || message_bytes_since_commit > 0
        || rebuild_state.pending.is_some()
    {
        commit_rebuild_progress(
            offset,
            processed_conversations,
            indexed_docs,
            &mut rebuild_state,
            &mut t_index,
        )?;
    }

    storage.close().with_context(|| {
        format!(
            "closing readonly database after Tantivy rebuild: {}",
            db_path.display()
        )
    })?;
    rebuild_state.mark_completed(index_meta_fingerprint(&index_path)?);
    persist_lexical_rebuild_state(&index_path, &rebuild_state)?;

    if let Some(p) = &progress {
        p.phase.store(0, Ordering::Relaxed);
        p.is_rebuilding.store(false, Ordering::Relaxed);
    }

    Ok(indexed_docs)
}

fn ingest_batch(
    storage: &FrankenStorage,
    t_index: &mut TantivyIndex,
    convs: &[NormalizedConversation],
    progress: &Option<Arc<IndexingProgress>>,
    force_tantivy_reindex: bool,
) -> Result<()> {
    // Use batched insert for better SQLite performance (single transaction)
    // This also handles daily_stats updates incrementally via InsertOutcome deltas.
    persist::persist_conversations_batched(storage, t_index, convs, force_tantivy_reindex)?;

    // Update progress counter for all conversations at once
    if let Some(p) = progress {
        p.current.fetch_add(convs.len(), Ordering::Relaxed);
    }
    Ok(())
}

/// Get all available connector factories.
///
/// Delegates to `franken_agent_detection::get_connector_factories()`.
pub use crate::connectors::get_connector_factories;

/// Detect all active roots for watching/scanning.
///
/// Includes:
/// 1. Local roots detected by connectors
/// 2. Remote mirror roots (assigned to ALL connectors since we don't know the mapping)
fn build_watch_roots(remote_roots: Vec<ScanRoot>) -> Vec<(ConnectorKind, ScanRoot)> {
    let factories = get_connector_factories();
    let mut roots = Vec::new();
    let mut all_kinds = Vec::new();

    for (name, factory) in factories {
        if let Some(kind) = ConnectorKind::from_slug(name) {
            all_kinds.push(kind);
            let conn = factory();
            let detection = conn.detect();
            if detection.detected {
                for root_path in detection.root_paths {
                    roots.push((kind, ScanRoot::local(root_path)));
                }
            }
        }
    }

    // Add remote roots for ALL connectors
    for remote_root in remote_roots {
        for kind in &all_kinds {
            roots.push((*kind, remote_root.clone()));
        }
    }

    roots
}

impl ConnectorKind {
    fn from_slug(slug: &str) -> Option<Self> {
        match slug {
            "codex" => Some(Self::Codex),
            "cline" => Some(Self::Cline),
            "gemini" => Some(Self::Gemini),
            "claude" => Some(Self::Claude),
            "clawdbot" => Some(Self::Clawdbot),
            "vibe" => Some(Self::Vibe),
            "amp" => Some(Self::Amp),
            "opencode" => Some(Self::OpenCode),
            "aider" => Some(Self::Aider),
            "cursor" => Some(Self::Cursor),
            "chatgpt" => Some(Self::ChatGpt),
            "pi_agent" => Some(Self::PiAgent),
            "factory" => Some(Self::Factory),
            "openclaw" => Some(Self::OpenClaw),
            "copilot" => Some(Self::Copilot),
            "kimi" => Some(Self::Kimi),
            "copilot_cli" => Some(Self::CopilotCli),
            "qwen" => Some(Self::Qwen),
            _ => None,
        }
    }

    /// Create a boxed connector instance for this kind.
    /// Centralizes connector instantiation to avoid duplicate match arms.
    fn create_connector(&self) -> Box<dyn Connector + Send> {
        match self {
            Self::Codex => Box::new(CodexConnector::new()),
            Self::Cline => Box::new(ClineConnector::new()),
            Self::Gemini => Box::new(GeminiConnector::new()),
            Self::Claude => Box::new(ClaudeCodeConnector::new()),
            Self::Clawdbot => Box::new(ClawdbotConnector::new()),
            Self::Vibe => Box::new(VibeConnector::new()),
            Self::Amp => Box::new(AmpConnector::new()),
            Self::OpenCode => Box::new(OpenCodeConnector::new()),
            Self::Aider => Box::new(AiderConnector::new()),
            Self::Cursor => Box::new(CursorConnector::new()),
            Self::ChatGpt => Box::new(ChatGptConnector::new()),
            Self::PiAgent => Box::new(PiAgentConnector::new()),
            Self::Factory => Box::new(FactoryConnector::new()),
            Self::OpenClaw => Box::new(OpenClawConnector::new()),
            Self::Copilot => Box::new(CopilotConnector::new()),
            Self::Kimi => Box::new(KimiConnector::new()),
            Self::CopilotCli => Box::new(CopilotCliConnector::new()),
            Self::Qwen => Box::new(QwenConnector::new()),
        }
    }
}

fn watch_sources<F: Fn(Vec<PathBuf>, &[(ConnectorKind, ScanRoot)], bool)>(
    watch_once_paths: Option<Vec<PathBuf>>,
    roots: Vec<(ConnectorKind, ScanRoot)>,
    event_channel: Option<(Sender<IndexerEvent>, Receiver<IndexerEvent>)>,
    stale_detector: Arc<StaleDetector>,
    watch_interval_secs: u64,
    callback: F,
) -> Result<()> {
    if let Some(paths) = watch_once_paths {
        if !paths.is_empty() {
            callback(paths, &roots, false);
        }
        return Ok(());
    }

    let (tx, rx) = event_channel.unwrap_or_else(crossbeam_channel::unbounded);
    let tx_clone = tx.clone();

    let mut watcher = recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            if event.need_rescan() {
                let _ = tx_clone.send(IndexerEvent::Command(ReindexCommand::Full));
                return;
            }
            if !watch_event_should_trigger_reindex(&event) || event.paths.is_empty() {
                return;
            }
            let _ = tx_clone.send(IndexerEvent::Notify(event.paths));
        }
    })?;

    // Watch all detected roots
    for (_, root) in &roots {
        if let Err(e) = watcher.watch(&root.path, RecursiveMode::Recursive) {
            tracing::warn!("failed to watch {}: {}", root.path.display(), e);
        } else {
            tracing::info!("watching {}", root.path.display());
        }
    }

    let debounce = Duration::from_secs(2);
    let max_wait = Duration::from_secs(5);
    // Minimum interval between scan cycles to prevent tight-loop CPU burn
    // when filesystem events arrive continuously. Default: 30s. (Issue #129)
    let min_scan_interval = Duration::from_secs(watch_interval_secs.max(1));
    // Stale check interval: check every 5 minutes for quicker detection
    let stale_check_interval = Duration::from_secs(300);
    let mut pending: Vec<PathBuf> = Vec::new();
    let mut first_event: Option<Instant> = None;
    let mut last_stale_check = Instant::now();
    // Initialize to the past so the first scan can fire immediately.
    // Use checked_sub to avoid panic if system uptime < min_scan_interval
    // (e.g., --watch-interval 999999 on a freshly booted system).
    // If the full interval won't fit, try smaller values so the first scan
    // still fires quickly rather than waiting the full cooldown.
    let mut last_scan = [
        min_scan_interval,
        Duration::from_secs(60),
        Duration::from_secs(1),
    ]
    .iter()
    .find_map(|d| Instant::now().checked_sub(*d))
    .unwrap_or_else(Instant::now);

    tracing::info!(
        watch_interval_secs,
        "watch mode: minimum interval between scan cycles"
    );

    loop {
        // How much cooldown remains before we may fire the next callback.
        // Using this as recv_timeout lets us keep accumulating events
        // instead of blocking with thread::sleep (which would drop events
        // if the inotify buffer fills up).
        let cooldown_remaining = min_scan_interval.saturating_sub(last_scan.elapsed());

        // Calculate timeout: use stale check interval when idle, debounce when active
        let timeout = if pending.is_empty() {
            stale_check_interval
        } else {
            let now = Instant::now();
            let elapsed = now.duration_since(first_event.unwrap_or(now));
            if elapsed >= max_wait {
                if cooldown_remaining.is_zero() {
                    // Cooldown elapsed and max_wait exceeded: fire now.
                    callback(std::mem::take(&mut pending), &roots, false);
                    last_scan = Instant::now();
                    first_event = None;
                    continue;
                }
                // max_wait exceeded but cooldown still active: wait for
                // the remaining cooldown while accumulating new events.
                cooldown_remaining
            } else {
                let remaining = max_wait.saturating_sub(elapsed);
                // Use the larger of (debounce, cooldown_remaining) to ensure
                // we never fire the callback faster than min_scan_interval.
                debounce.min(remaining).max(cooldown_remaining)
            }
        };

        match rx.recv_timeout(timeout) {
            Ok(IndexerEvent::Notify(paths)) => {
                if pending.is_empty() {
                    first_event = Some(Instant::now());
                }
                pending.extend(paths);
            }
            Ok(IndexerEvent::Command(cmd)) => match cmd {
                ReindexCommand::Full => {
                    // Full rebuild commands bypass cooldown for responsive
                    // operator-initiated rebuilds.
                    if !pending.is_empty() {
                        callback(std::mem::take(&mut pending), &roots, false);
                    }
                    callback(vec![], &roots, true);
                    last_scan = Instant::now();
                    first_event = None;
                }
            },
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                // Process pending events only if cooldown has elapsed
                if !pending.is_empty() && last_scan.elapsed() >= min_scan_interval {
                    callback(std::mem::take(&mut pending), &roots, false);
                    last_scan = Instant::now();
                    first_event = None;
                }

                // Periodic stale check
                let now = Instant::now();
                if now.duration_since(last_stale_check) >= stale_check_interval {
                    last_stale_check = now;

                    if let Some(action) = stale_detector.check_stale() {
                        let stats = stale_detector.stats();
                        match action {
                            StaleAction::Warn => {
                                tracing::warn!(
                                    consecutive_zero_scans = stats.consecutive_zero_scans,
                                    seconds_since_last_ingest = ?stats.seconds_since_last_ingest,
                                    total_ingests = stats.total_ingests,
                                    "watch daemon appears stale: no conversations indexed recently"
                                );
                                tracing::info!(
                                    "hint: run 'cass index --full' to rebuild, or set \
                                     CASS_WATCH_STALE_ACTION=rebuild for auto-recovery"
                                );
                            }
                            StaleAction::Rebuild => {
                                tracing::warn!(
                                    consecutive_zero_scans = stats.consecutive_zero_scans,
                                    seconds_since_last_ingest = ?stats.seconds_since_last_ingest,
                                    "stale state detected, triggering automatic full rebuild"
                                );
                                // Trigger full rebuild
                                callback(vec![], &roots, true);
                                last_scan = Instant::now();
                            }
                            StaleAction::None => {
                                // Stale detection disabled, should not reach here
                            }
                        }
                    }
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

#[cfg(test)]
fn reset_storage(storage: &FrankenStorage, db_path: &Path) -> Result<()> {
    // Wrap the canonical-table reset in a transaction so partial clears roll back.
    // The derived FTS table is recreated explicitly afterward because the
    // frankensqlite writer path does not implement the FTS5 control-column
    // `delete-all` command used by stock SQLite.
    storage.raw().execute_batch(
        "BEGIN TRANSACTION;
         DELETE FROM usage_models_daily;
         DELETE FROM usage_daily;
         DELETE FROM usage_hourly;
         DELETE FROM token_daily_stats;
         DELETE FROM daily_stats;
         DELETE FROM message_metrics;
         DELETE FROM token_usage;
         DELETE FROM snippets;
         DELETE FROM messages;
         DELETE FROM conversations;
         DELETE FROM agents;
         DELETE FROM workspaces;
         DELETE FROM tags;
         DELETE FROM conversation_tags;
         DELETE FROM meta WHERE key = 'last_scan_ts';
         COMMIT;",
    )?;
    let _ = storage
        .raw()
        .execute_batch("DROP TABLE IF EXISTS fts_messages;");
    crate::storage::sqlite::materialize_fresh_fts_schema_via_rusqlite(db_path)?;
    Ok(())
}

/// Reindex paths and return the total number of conversations indexed.
///
/// Returns `Ok(count)` where count is the number of conversations successfully indexed.
/// This count is used by the stale detector to track indexing activity.
fn reindex_paths(
    opts: &IndexOptions,
    paths: Vec<PathBuf>,
    roots: &[(ConnectorKind, ScanRoot)],
    state: &Mutex<HashMap<ConnectorKind, i64>>,
    storage: &Mutex<FrankenStorage>,
    t_index: &Mutex<TantivyIndex>,
    force_full: bool,
) -> Result<usize> {
    // DO NOT lock storage/index here for the whole duration.
    // We only need them for the ingest phase, not the scan phase.

    let triggers = classify_paths(paths, roots);
    if triggers.is_empty() {
        return Ok(0);
    }

    let mut total_indexed = 0usize;

    for (kind, root, min_ts, max_ts) in triggers {
        let conn = kind.create_connector();
        let detect = conn.detect();
        if !detect.detected && root.origin.source_id == "local" && !root.path.exists() {
            // For local roots, if detection fails and the root is gone, skip.
            // For remote roots, detection might fail but we should still try scanning
            // if it's a brute-force attempt.
            continue;
        }

        // Update phase to scanning
        if let Some(p) = &opts.progress {
            p.phase.store(1, Ordering::Relaxed);
        }

        let since_ts = if force_full {
            None
        } else {
            let guard = state
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            let previous_ts = guard.get(&kind).copied();
            match (previous_ts, min_ts) {
                // No previous watermark and no trigger timestamp: full scan this root.
                (None, None) => None,
                // Only one side available: use it.
                (Some(ts), None) | (None, Some(ts)) => Some(ts.saturating_sub(1)),
                // Use the older timestamp so out-of-order file events are not skipped.
                (Some(prev), Some(batch_min)) => Some(prev.min(batch_min).saturating_sub(1)),
            }
        };

        // Use explicit root context
        let ctx = crate::connectors::ScanContext::with_roots(
            root.path.clone(),
            vec![root.clone()],
            since_ts,
        );

        // SCAN PHASE: IO-heavy, no locks held
        let mut convs = match conn.scan(&ctx) {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(
                    "watch scan failed for {:?} at {}: {}",
                    kind,
                    root.path.display(),
                    e
                );
                Vec::new()
            }
        };

        // Provenance injection and path rewriting
        for conv in &mut convs {
            inject_provenance(conv, &root.origin);
            apply_workspace_rewrite(conv, &root);
        }

        // Update total and phase to indexing
        if let Some(p) = &opts.progress {
            p.total.fetch_add(convs.len(), Ordering::Relaxed);
            p.phase.store(2, Ordering::Relaxed);
        }

        let conv_count = convs.len();
        tracing::info!(?kind, conversations = conv_count, since_ts, "watch_scan");

        // INGEST PHASE: Acquire locks briefly
        {
            let storage = storage
                .lock()
                .map_err(|_| anyhow::anyhow!("storage lock poisoned"))?;
            let mut t_index = t_index
                .lock()
                .map_err(|_| anyhow::anyhow!("index lock poisoned"))?;

            ingest_batch(&storage, &mut t_index, &convs, &opts.progress, false)?;

            // Commit to Tantivy immediately to ensure index consistency before advancing watch state.
            t_index.commit()?;

            // Keep last_indexed_at current so `cass status` doesn't report stale during watch mode
            storage.set_last_indexed_at(FrankenStorage::now_millis())?;
        }

        // Track total indexed for stale detection
        total_indexed += conv_count;

        if conv_count > 0
            && let Some(ts_val) = max_ts
        {
            let mut guard = state
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            let entry = guard.entry(kind).or_insert(ts_val);
            // Use max_ts for state update (high water mark)
            *entry = (*entry).max(ts_val);
            save_watch_state(&opts.data_dir, &guard)?;
        }
    }

    // Reset phase to idle if progress exists
    reset_progress_to_idle(opts.progress.as_ref());

    Ok(total_indexed)
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ConnectorKind {
    #[serde(rename = "cx", alias = "Codex")]
    Codex,
    #[serde(rename = "cl", alias = "Cline")]
    Cline,
    #[serde(rename = "gm", alias = "Gemini")]
    Gemini,
    #[serde(rename = "cd", alias = "Claude")]
    Claude,
    #[serde(rename = "cb", alias = "Clawdbot")]
    Clawdbot,
    #[serde(rename = "vb", alias = "Vibe")]
    Vibe,
    #[serde(rename = "am", alias = "Amp")]
    Amp,
    #[serde(rename = "oc", alias = "OpenCode")]
    OpenCode,
    #[serde(rename = "ai", alias = "Aider")]
    Aider,
    #[serde(rename = "cu", alias = "Cursor")]
    Cursor,
    #[serde(rename = "cg", alias = "ChatGpt")]
    ChatGpt,
    #[serde(rename = "pi", alias = "PiAgent")]
    PiAgent,
    #[serde(rename = "fa", alias = "Factory")]
    Factory,
    #[serde(rename = "ow", alias = "OpenClaw")]
    OpenClaw,
    #[serde(rename = "cp", alias = "Copilot")]
    Copilot,
    #[serde(rename = "ki", alias = "Kimi")]
    Kimi,
    #[serde(rename = "cc", alias = "CopilotCli")]
    CopilotCli,
    #[serde(rename = "qw", alias = "Qwen")]
    Qwen,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
struct WatchState {
    #[serde(rename = "v", default, skip_serializing_if = "is_zero_u8")]
    version: u8,
    #[serde(rename = "m", default, skip_serializing_if = "HashMap::is_empty")]
    map: HashMap<ConnectorKind, i64>,
}

fn is_zero_u8(value: &u8) -> bool {
    *value == 0
}

fn state_path(data_dir: &Path) -> PathBuf {
    data_dir.join("watch_state.json")
}

fn load_watch_state(data_dir: &Path) -> HashMap<ConnectorKind, i64> {
    let path = state_path(data_dir);
    let Ok(bytes) = fs::read(&path) else {
        return HashMap::new();
    };

    if let Ok(state) = serde_json::from_slice::<WatchState>(&bytes) {
        return state.map;
    }

    if let Ok(map) = serde_json::from_slice::<HashMap<ConnectorKind, i64>>(&bytes) {
        return map;
    }
    HashMap::new()
}

fn replace_file_from_temp(temp_path: &Path, final_path: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        match fs::rename(temp_path, final_path) {
            Ok(()) => Ok(()),
            Err(first_err)
                if final_path.exists()
                    && matches!(
                        first_err.kind(),
                        std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::PermissionDenied
                    ) =>
            {
                let backup_path = unique_replace_backup_path(final_path);
                fs::rename(final_path, &backup_path).map_err(|backup_err| {
                    let _ = fs::remove_file(temp_path);
                    anyhow::anyhow!(
                        "failed preparing backup {} before replacing {}: first error: {}; backup error: {}",
                        backup_path.display(),
                        final_path.display(),
                        first_err,
                        backup_err
                    )
                })?;
                match fs::rename(temp_path, final_path) {
                    Ok(()) => {
                        let _ = fs::remove_file(&backup_path);
                        Ok(())
                    }
                    Err(second_err) => {
                        let restore_result = fs::rename(&backup_path, final_path);
                        match restore_result {
                            Ok(()) => {
                                let _ = fs::remove_file(temp_path);
                                Err(anyhow::anyhow!(
                                    "failed replacing {} with {}: first error: {}; second error: {}; restored original file",
                                    final_path.display(),
                                    temp_path.display(),
                                    first_err,
                                    second_err
                                ))
                            }
                            Err(restore_err) => Err(anyhow::anyhow!(
                                "failed replacing {} with {}: first error: {}; second error: {}; restore error: {}; temp file retained at {}",
                                final_path.display(),
                                temp_path.display(),
                                first_err,
                                second_err,
                                restore_err,
                                temp_path.display()
                            )),
                        }
                    }
                }
            }
            Err(rename_err) => Err(rename_err.into()),
        }
    }

    #[cfg(not(windows))]
    {
        fs::rename(temp_path, final_path)?;
        Ok(())
    }
}

fn unique_atomic_temp_path(path: &Path) -> PathBuf {
    unique_atomic_sidecar_path(path, "tmp", "watch_state.json")
}

#[cfg(windows)]
fn unique_replace_backup_path(path: &Path) -> PathBuf {
    unique_atomic_sidecar_path(path, "bak", "watch_state.json")
}

fn unique_atomic_sidecar_path(path: &Path, suffix: &str, fallback_name: &str) -> PathBuf {
    static NEXT_NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let nonce = NEXT_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(fallback_name);

    path.with_file_name(format!(
        ".{file_name}.{suffix}.{}.{}.{}",
        std::process::id(),
        timestamp,
        nonce
    ))
}

fn save_watch_state(data_dir: &Path, state: &HashMap<ConnectorKind, i64>) -> Result<()> {
    let path = state_path(data_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let watch_state = WatchState {
        version: 1,
        map: state.clone(),
    };
    let json = serde_json::to_vec(&watch_state)?;
    // Atomic write: write to temp file then rename, so a crash mid-write
    // cannot leave a truncated/corrupt watch_state.json.
    let tmp_path = unique_atomic_temp_path(&path);
    fs::write(&tmp_path, json)?;
    replace_file_from_temp(&tmp_path, &path)?;
    Ok(())
}

fn set_progress_last_error(progress: Option<&Arc<IndexingProgress>>, error: Option<String>) {
    let Some(progress) = progress else {
        return;
    };

    match progress.last_error.lock() {
        Ok(mut guard) => *guard = error,
        Err(poisoned) => *poisoned.into_inner() = error,
    }
}

fn finalize_watch_reindex_result(
    result: Result<usize>,
    detector: &StaleDetector,
    progress: Option<&Arc<IndexingProgress>>,
    context: &str,
) -> usize {
    match result {
        Ok(indexed) => {
            set_progress_last_error(progress, None);
            detector.record_scan(indexed);
            indexed
        }
        Err(error) => {
            tracing::warn!(error = %error, context, "watch reindex failed");
            reset_progress_to_idle(progress);
            set_progress_last_error(progress, Some(format!("{context}: {error}")));
            detector.record_scan(0);
            0
        }
    }
}

fn classify_paths(
    paths: Vec<PathBuf>,
    roots: &[(ConnectorKind, ScanRoot)],
) -> Vec<(ConnectorKind, ScanRoot, Option<i64>, Option<i64>)> {
    // Key -> (Root, MinTS, MaxTS)
    let mut batch_map: BatchClassificationMap = HashMap::new();

    for p in paths {
        if let Ok(meta) = std::fs::metadata(&p)
            && let Ok(time) = meta.modified()
            && let Ok(dur) = time.duration_since(std::time::UNIX_EPOCH)
        {
            let ts = Some(i64::try_from(dur.as_millis()).unwrap_or(i64::MAX));

            // Find ALL matching roots
            for (kind, root) in roots {
                if p.starts_with(&root.path) {
                    let key = (*kind, root.path.clone());
                    let entry = batch_map.entry(key).or_insert((root.clone(), None, None));

                    // Update MinTS (for scan window start)
                    entry.1 = match (entry.1, ts) {
                        (Some(prev), Some(cur)) => Some(prev.min(cur)),
                        (None, Some(cur)) => Some(cur),
                        _ => entry.1,
                    };

                    // Update MaxTS (for state high-water mark)
                    entry.2 = match (entry.2, ts) {
                        (Some(prev), Some(cur)) => Some(prev.max(cur)),
                        (None, Some(cur)) => Some(cur),
                        _ => entry.2,
                    };
                }
            }
        }
    }

    batch_map
        .into_iter()
        .map(|((kind, _), (root, min_ts, max_ts))| (kind, root, min_ts, max_ts))
        .collect()
}

fn watch_event_should_trigger_reindex(event: &notify::Event) -> bool {
    match event.kind {
        notify::event::EventKind::Access(AccessKind::Close(AccessMode::Write)) => true,
        notify::event::EventKind::Access(_) => false,
        notify::event::EventKind::Create(_)
        | notify::event::EventKind::Any
        | notify::event::EventKind::Other => true,
        // Incremental watch indexing is append-only today: once a path is gone,
        // classify_paths() cannot derive a scan window from it and the ingest
        // path cannot delete the stale conversation rows it previously indexed.
        // Treat remove events as noise until delete-aware rebuilds exist.
        notify::event::EventKind::Remove(_) => false,
        notify::event::EventKind::Modify(ModifyKind::Metadata(MetadataKind::AccessTime)) => false,
        notify::event::EventKind::Modify(_) => true,
    }
}

fn sync_sources_config_to_db(storage: &FrankenStorage) {
    if dotenvy::var("CASS_IGNORE_SOURCES_CONFIG").is_ok() {
        return;
    }
    let config = match SourcesConfig::load() {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::debug!("sources config load failed: {e}");
            return;
        }
    };

    for source in config.remote_sources() {
        let platform = source.platform.map(|p| match p {
            Platform::Macos => "macos".to_string(),
            Platform::Linux => "linux".to_string(),
            Platform::Windows => "windows".to_string(),
        });

        let config_json = serde_json::json!({
            "paths": source.paths.clone(),
            "path_mappings": source.path_mappings.clone(),
            "sync_schedule": source.sync_schedule,
        });

        let record = Source {
            id: source.name.clone(),
            kind: source.source_type,
            host_label: source.host.clone(),
            machine_id: None,
            platform,
            config_json: Some(config_json),
            created_at: None,
            updated_at: None,
        };

        if let Err(e) = storage.upsert_source(&record) {
            tracing::warn!(
                source_id = %record.id,
                "failed to upsert source into db: {e}"
            );
        }
    }
}

/// Build a list of scan roots for multi-root indexing.
///
/// This function collects both:
/// 1. Local default roots (from watch_roots() or standard locations)
/// 2. Remote mirror roots (from registered sources in the database)
///
/// Part of P2.2 - Indexer multi-root orchestration.
pub fn build_scan_roots(storage: &FrankenStorage, data_dir: &Path) -> Vec<ScanRoot> {
    let mut roots = Vec::new();

    // Add local default root with local provenance
    // We create a single "local" root that encompasses all local paths.
    // Connectors will use their own default detection logic when given an empty scan_roots.
    // For explicit multi-root support, we add the local root.
    roots.push(ScanRoot::local(data_dir.to_path_buf()));

    if dotenvy::var("CASS_IGNORE_SOURCES_CONFIG").is_err()
        && let Ok(config) = SourcesConfig::load()
    {
        let remotes: Vec<_> = config.remote_sources().collect();
        if !remotes.is_empty() {
            for source in remotes {
                let origin = Origin {
                    source_id: source.name.clone(),
                    kind: source.source_type,
                    host: source.host.clone(),
                };
                let platform = source.platform;
                let workspace_rewrites = source.path_mappings.clone();

                for path in &source.paths {
                    // Generate safe dirname from the path as configured
                    let expanded_path = if path.starts_with("~/") {
                        path.to_string()
                    } else if path.starts_with('~') {
                        path.replacen('~', "~/", 1)
                    } else {
                        path.to_string()
                    };
                    let safe_name = path_to_safe_dirname(&expanded_path);
                    let mirror_base = data_dir.join("remotes").join(&source.name).join("mirror");
                    let mirror_path = mirror_base.join(&safe_name);

                    // Try the direct path first
                    if mirror_path.exists() {
                        let mut scan_root = ScanRoot::remote(mirror_path, origin.clone(), platform);
                        scan_root.workspace_rewrites = workspace_rewrites.clone();
                        roots.push(scan_root);
                        continue;
                    }

                    // Fallback: sync may have expanded ~ differently (issue #45)
                    // Look for any directory in mirror that ends with the path suffix
                    // e.g., "~/.claude/projects" -> find "*_.claude_projects"
                    if path.starts_with("~/") {
                        let suffix = path.trim_start_matches("~/");
                        let safe_suffix = path_to_safe_dirname(suffix);
                        if let Ok(entries) = std::fs::read_dir(&mirror_base) {
                            for entry in entries.flatten() {
                                let name = entry.file_name();
                                let name_str = name.to_string_lossy();
                                // Match directories ending with the expected suffix
                                // e.g., "home_user_.claude_projects" ends with ".claude_projects"
                                if name_str.ends_with(&safe_suffix) && entry.path().is_dir() {
                                    let mut scan_root =
                                        ScanRoot::remote(entry.path(), origin.clone(), platform);
                                    scan_root.workspace_rewrites = workspace_rewrites.clone();
                                    roots.push(scan_root);
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            return roots;
        }
    }

    // Fallback: remote mirror roots from registered sources
    if let Ok(sources) = storage.list_sources() {
        for source in sources {
            // Skip local source - already handled above
            if !source.kind.is_remote() {
                continue;
            }

            // Parse platform from source
            let platform =
                source
                    .platform
                    .as_deref()
                    .and_then(|p| match p.to_lowercase().as_str() {
                        "macos" => Some(Platform::Macos),
                        "linux" => Some(Platform::Linux),
                        "windows" => Some(Platform::Windows),
                        _ => None,
                    });

            // Parse workspace rewrites from config_json
            // Format: array of {from, to, agents?} objects
            let workspace_rewrites = source
                .config_json
                .as_ref()
                .and_then(|cfg| cfg.get("path_mappings"))
                .and_then(|arr| arr.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| {
                            let from = item.get("from")?.as_str()?.to_string();
                            let to = item.get("to")?.as_str()?.to_string();
                            let agents = item.get("agents").and_then(|a| {
                                a.as_array().map(|arr| {
                                    arr.iter()
                                        .filter_map(|v| v.as_str().map(String::from))
                                        .collect()
                                })
                            });
                            Some(crate::sources::config::PathMapping { from, to, agents })
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();

            if let Some(paths) = source
                .config_json
                .as_ref()
                .and_then(|cfg| cfg.get("paths"))
                .and_then(|arr| arr.as_array())
            {
                for path_val in paths {
                    let Some(path) = path_val.as_str() else {
                        continue;
                    };
                    let expanded_path = if path.starts_with("~/") {
                        path.to_string()
                    } else if path.starts_with('~') {
                        path.replacen('~', "~/", 1)
                    } else {
                        path.to_string()
                    };
                    let safe_name = path_to_safe_dirname(&expanded_path);
                    let mirror_path = data_dir
                        .join("remotes")
                        .join(&source.id)
                        .join("mirror")
                        .join(&safe_name);
                    if !mirror_path.exists() {
                        continue;
                    }

                    let origin = Origin {
                        source_id: source.id.clone(),
                        kind: source.kind,
                        host: source.host_label.clone(),
                    };
                    let mut scan_root = ScanRoot::remote(mirror_path, origin, platform);
                    scan_root.workspace_rewrites = workspace_rewrites.clone();
                    roots.push(scan_root);
                }
                continue;
            }

            // Remote mirror directory: data_dir/remotes/<source_id>/mirror
            let mirror_path = data_dir.join("remotes").join(&source.id).join("mirror");

            if mirror_path.exists() {
                let origin = Origin {
                    source_id: source.id.clone(),
                    kind: source.kind,
                    host: source.host_label.clone(),
                };
                let mut scan_root = ScanRoot::remote(mirror_path, origin, platform);
                scan_root.workspace_rewrites = workspace_rewrites;

                roots.push(scan_root);
            }
        }
    }

    roots
}

/// Inject provenance metadata into a conversation from a scan root's origin.
///
/// This adds the `cass.origin` field to the conversation's metadata JSON
/// so that persistence can extract and store the source_id.
///
/// Part of P2.2 - provenance injection.
fn inject_provenance(conv: &mut NormalizedConversation, origin: &Origin) {
    // Ensure metadata is an object
    if !conv.metadata.is_object() {
        conv.metadata = serde_json::json!({});
    }

    // Add cass.origin provenance
    if let Some(obj) = conv.metadata.as_object_mut() {
        let cass = obj
            .entry("cass".to_string())
            .or_insert_with(|| serde_json::json!({}));
        if let Some(cass_obj) = cass.as_object_mut() {
            cass_obj.insert(
                "origin".to_string(),
                serde_json::json!({
                    "source_id": origin.source_id,
                    "kind": origin.kind.as_str(),
                    "host": origin.host
                }),
            );
        }
    }
}

/// Apply workspace path rewriting to a conversation.
///
/// This rewrites workspace paths from remote formats to local equivalents
/// at ingest time, ensuring that workspace filters work consistently
/// across local and remote sources.
///
/// The original workspace path is preserved in metadata.cass.workspace_original
/// for display/audit purposes.
///
/// Part of P6.2 - Apply path mappings at ingest time.
pub fn apply_workspace_rewrite(conv: &mut NormalizedConversation, root: &ScanRoot) {
    // Only apply if we have a workspace and rewrites
    if root.workspace_rewrites.is_empty() {
        return;
    }

    // Clone workspace upfront to avoid borrow issues
    let original_workspace = match &conv.workspace {
        Some(ws) => ws.to_string_lossy().to_string(),
        None => return,
    };

    // Use optimized trie lookup via ScanRoot
    let rewritten = root.rewrite_workspace(&original_workspace, Some(&conv.agent_slug));

    // Only proceed if the rewrite actually changed something
    if rewritten != original_workspace {
        // Store original in metadata
        if !conv.metadata.is_object() {
            conv.metadata = serde_json::json!({});
        }

        if let Some(obj) = conv.metadata.as_object_mut() {
            // Get or create cass object
            let cass = obj
                .entry("cass".to_string())
                .or_insert_with(|| serde_json::json!({}));
            if let Some(cass_obj) = cass.as_object_mut() {
                cass_obj.insert(
                    "workspace_original".to_string(),
                    serde_json::Value::String(original_workspace.clone()),
                );
            }
        }

        // Update workspace to rewritten path
        conv.workspace = Some(std::path::PathBuf::from(&rewritten));

        tracing::debug!(
            original = %original_workspace,
            rewritten = %rewritten,
            agent = %conv.agent_slug,
            "workspace_rewritten"
        );
    }
}

pub mod persist {
    use std::collections::{HashMap, HashSet};
    use std::time::Duration;

    use anyhow::{Context, Result, anyhow};
    use frankensqlite::FrankenError;
    use rayon::prelude::*;

    use crate::connectors::NormalizedConversation;
    use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole, Snippet};
    use crate::search::tantivy::TantivyIndex;
    use crate::storage::sqlite::{FrankenStorage, IndexingCache, InsertOutcome};

    fn begin_concurrent_writes_enabled() -> bool {
        dotenvy::var("CASS_INDEXER_BEGIN_CONCURRENT")
            .map(|v| !(v == "0" || v.eq_ignore_ascii_case("false")))
            .unwrap_or(false)
    }

    pub(super) fn begin_concurrent_retry_limit() -> usize {
        dotenvy::var("CASS_INDEXER_BEGIN_CONCURRENT_RETRIES")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(6)
    }

    fn begin_concurrent_chunk_size() -> usize {
        dotenvy::var("CASS_INDEXER_BEGIN_CONCURRENT_CHUNK_SIZE")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(32)
    }

    fn begin_concurrent_writer_cache_kib() -> i64 {
        dotenvy::var("CASS_INDEXER_BEGIN_CONCURRENT_WRITER_CACHE_KIB")
            .ok()
            .and_then(|v| v.parse::<i64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(4096)
    }

    fn index_writer_busy_timeout_ms() -> u64 {
        dotenvy::var("CASS_INDEX_WRITER_BUSY_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(60_000)
    }

    fn apply_begin_concurrent_writer_tuning(storage: &FrankenStorage) {
        let cache_kib = begin_concurrent_writer_cache_kib();
        let pragma = format!("PRAGMA cache_size = -{cache_kib};");
        if let Err(err) = storage.raw().execute(&pragma) {
            tracing::debug!(
                cache_kib,
                error = %err,
                "failed_to_apply_begin_concurrent_writer_cache_size"
            );
        }
    }

    pub(super) fn apply_index_writer_busy_timeout(storage: &FrankenStorage) {
        let busy_timeout_ms = index_writer_busy_timeout_ms();
        let pragma = format!("PRAGMA busy_timeout = {busy_timeout_ms};");
        if let Err(err) = storage.raw().execute(&pragma) {
            tracing::debug!(
                busy_timeout_ms,
                error = %err,
                "failed_to_apply_index_writer_busy_timeout"
            );
        }
    }

    fn transient_franken_error(err: &anyhow::Error) -> Option<&FrankenError> {
        err.downcast_ref::<FrankenError>()
            .or_else(|| err.root_cause().downcast_ref::<FrankenError>())
    }

    fn is_retryable_franken_error(err: &anyhow::Error) -> bool {
        transient_franken_error(err).is_some_and(|inner| {
            matches!(
                inner,
                FrankenError::Busy
                    | FrankenError::BusyRecovery
                    | FrankenError::BusySnapshot { .. }
                    | FrankenError::WriteConflict { .. }
                    | FrankenError::SerializationFailure { .. }
            )
        })
    }

    /// Retry wrapper for any retryable FrankenError (BusySnapshot, WriteConflict, etc.)
    pub(super) fn with_concurrent_retry<F, T>(max_retries: usize, mut f: F) -> Result<T>
    where
        F: FnMut() -> Result<T>,
    {
        let mut backoff_ms = 4_u64;
        for attempt in 0..=max_retries {
            match f() {
                Ok(val) => return Ok(val),
                Err(err) if attempt < max_retries && is_retryable_franken_error(&err) => {
                    tracing::debug!(
                        attempt = attempt + 1,
                        max_retries,
                        backoff_ms,
                        error = %err,
                        "begin_concurrent_retry"
                    );
                    std::thread::sleep(Duration::from_millis(backoff_ms));
                    backoff_ms = (backoff_ms * 2).min(128);
                }
                Err(err) => return Err(err),
            }
        }
        Err(anyhow!("exhausted begin-concurrent retries"))
    }

    fn duplicate_conversation_keys_present(convs: &[NormalizedConversation]) -> bool {
        let mut seen = HashSet::with_capacity(convs.len());
        for conv in convs {
            let (source_id, _) = extract_provenance(&conv.metadata);
            let key = if let Some(external_id) = conv.external_id.as_deref() {
                (
                    conv.agent_slug.clone(),
                    source_id,
                    Some(external_id.to_owned()),
                    None,
                    conv.started_at,
                )
            } else {
                (
                    conv.agent_slug.clone(),
                    source_id,
                    None,
                    Some(conv.source_path.to_string_lossy().to_string()),
                    None,
                )
            };
            if !seen.insert(key) {
                return true;
            }
        }
        false
    }

    fn persist_conversations_batched_begin_concurrent(
        db_path: &std::path::Path,
        t_index: &mut TantivyIndex,
        convs: &[NormalizedConversation],
        force_tantivy_reindex: bool,
    ) -> Result<()> {
        let max_retries = begin_concurrent_retry_limit();
        let chunk_size = begin_concurrent_chunk_size().min(convs.len().max(1));

        let indexed_chunks: Vec<Result<Vec<(usize, InsertOutcome)>>> = convs
            .par_chunks(chunk_size)
            .enumerate()
            .map(|(chunk_idx, chunk)| {
                let franken = FrankenStorage::open_writer(db_path).with_context(|| {
                    format!(
                        "opening frankensqlite writer for begin-concurrent mode: {}",
                        db_path.display()
                    )
                })?;
                apply_begin_concurrent_writer_tuning(&franken);
                let result: Result<Vec<(usize, InsertOutcome)>> = (|| {
                    let mut outcomes = Vec::with_capacity(chunk.len());
                    let mut agent_cache: HashMap<String, i64> = HashMap::new();
                    let mut workspace_cache: HashMap<std::path::PathBuf, i64> = HashMap::new();

                    for (offset, conv) in chunk.iter().enumerate() {
                        let idx = chunk_idx * chunk_size + offset;

                        // Wrap the entire ensure_agent + ensure_workspace +
                        // insert_conversation_tree sequence in the retry loop, since
                        // ensure_agent/workspace also write and can hit page conflicts.
                        let agent_slug = conv.agent_slug.clone();
                        let workspace = conv.workspace.clone();
                        let internal = map_to_internal(conv);

                        let outcome = with_concurrent_retry(max_retries, || {
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
                                let id = franken.ensure_agent(&agent)?;
                                agent_cache.insert(agent_slug.clone(), id);
                                id
                            };
                            let workspace_id = if let Some(ws) = &workspace {
                                if let Some(id) = workspace_cache.get(ws) {
                                    Some(*id)
                                } else {
                                    let id = franken.ensure_workspace(ws, None)?;
                                    workspace_cache.insert(ws.clone(), id);
                                    Some(id)
                                }
                            } else {
                                None
                            };
                            franken.insert_conversation_tree(agent_id, workspace_id, &internal)
                        })?;
                        outcomes.push((idx, outcome));
                    }

                    Ok(outcomes)
                })();
                let close_result = franken.close().with_context(|| {
                    format!(
                        "closing frankensqlite writer for begin-concurrent mode: {}",
                        db_path.display()
                    )
                });
                match result {
                    Ok(outcomes) => {
                        close_result?;
                        Ok(outcomes)
                    }
                    Err(err) => {
                        if let Err(close_err) = close_result {
                            tracing::warn!(
                                error = %close_err,
                                db_path = %db_path.display(),
                                "failed to close begin-concurrent writer cleanly after index error"
                            );
                        }
                        Err(err)
                    }
                }
            })
            .collect();

        let mut ordered = Vec::with_capacity(convs.len());
        for chunk in indexed_chunks {
            ordered.extend(chunk?);
        }
        ordered.sort_by_key(|(idx, _)| *idx);

        for (idx, outcome) in ordered {
            let conv = &convs[idx];
            if force_tantivy_reindex {
                t_index.add_messages(conv, &conv.messages)?;
            } else if !outcome.inserted_indices.is_empty() {
                let new_msgs: Vec<_> = conv
                    .messages
                    .iter()
                    .filter(|m| outcome.inserted_indices.contains(&m.idx))
                    .cloned()
                    .collect();
                t_index.add_messages(conv, &new_msgs)?;
            }
        }

        Ok(())
    }

    /// Extract provenance (source_id, origin_host) from conversation metadata.
    ///
    /// Looks for `metadata.cass.origin` object with source_id and host fields.
    /// Returns ("local", None) if no provenance is found.
    fn extract_provenance(metadata: &serde_json::Value) -> (String, Option<String>) {
        let source_id = metadata
            .get("cass")
            .and_then(|c| c.get("origin"))
            .and_then(|o| o.get("source_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("local")
            .to_string();

        let origin_host = metadata
            .get("cass")
            .and_then(|c| c.get("origin"))
            .and_then(|o| o.get("host"))
            .and_then(|v| v.as_str())
            .map(String::from);

        (source_id, origin_host)
    }

    /// Convert a NormalizedConversation to the internal Conversation type for SQLite storage.
    ///
    /// Extracts provenance from `metadata.cass.origin` if present, otherwise defaults to local.
    ///
    /// Applies secret redaction to message content and extra_json before storage
    /// (security fix for #112: tool-result secrets were persisted unredacted).
    pub fn map_to_internal(conv: &NormalizedConversation) -> Conversation {
        // Extract provenance from metadata (P2.2)
        let (source_id, origin_host) = extract_provenance(&conv.metadata);
        let should_redact = super::redact_secrets::redaction_enabled();

        Conversation {
            id: None,
            agent_slug: conv.agent_slug.clone(),
            workspace: conv.workspace.clone(),
            external_id: conv.external_id.clone(),
            title: conv.title.clone(),
            source_path: conv.source_path.clone(),
            started_at: conv.started_at,
            ended_at: conv.ended_at,
            approx_tokens: None,
            metadata_json: conv.metadata.clone(),
            messages: conv
                .messages
                .iter()
                .map(|m| {
                    let content = if should_redact {
                        super::redact_secrets::redact_text(&m.content)
                    } else {
                        m.content.clone()
                    };
                    let extra_json = if should_redact {
                        super::redact_secrets::redact_json(&m.extra)
                    } else {
                        m.extra.clone()
                    };
                    Message {
                        id: None,
                        idx: m.idx,
                        role: map_role(&m.role),
                        author: m.author.clone(),
                        created_at: m.created_at,
                        content,
                        extra_json,
                        snippets: m
                            .snippets
                            .iter()
                            .map(|s| Snippet {
                                id: None,
                                file_path: s.file_path.clone(),
                                start_line: s.start_line,
                                end_line: s.end_line,
                                language: s.language.clone(),
                                snippet_text: s.snippet_text.clone(),
                            })
                            .collect(),
                    }
                })
                .collect(),
            source_id,
            origin_host,
        }
    }

    pub fn persist_conversation(
        storage: &FrankenStorage,
        t_index: &mut TantivyIndex,
        conv: &NormalizedConversation,
    ) -> Result<()> {
        tracing::info!(agent = %conv.agent_slug, messages = conv.messages.len(), "persist_conversation");
        let agent = Agent {
            id: None,
            slug: conv.agent_slug.clone(),
            name: conv.agent_slug.clone(),
            version: None,
            kind: AgentKind::Cli,
        };
        let agent_id = storage.ensure_agent(&agent)?;

        let workspace_id = if let Some(ws) = &conv.workspace {
            Some(storage.ensure_workspace(ws, None)?)
        } else {
            None
        };

        let internal_conv = map_to_internal(conv);

        let InsertOutcome {
            conversation_id: _,
            inserted_indices,
        } = storage.insert_conversation_tree(agent_id, workspace_id, &internal_conv)?;

        // Only add newly inserted messages to the Tantivy index (incremental)
        if !inserted_indices.is_empty() {
            let new_msgs: Vec<_> = conv
                .messages
                .iter()
                .filter(|m| inserted_indices.contains(&m.idx))
                .cloned()
                .collect();
            t_index.add_messages(conv, &new_msgs)?;
        }
        Ok(())
    }

    /// Persist multiple conversations in a single database transaction for better performance.
    /// This reduces SQLite transaction overhead when indexing many conversations at once.
    ///
    /// Uses `IndexingCache` (Opt 7.2) to prevent N+1 queries for agent/workspace IDs.
    /// Set `CASS_SQLITE_CACHE=0` to disable caching for debugging.
    pub fn persist_conversations_batched(
        storage: &FrankenStorage,
        t_index: &mut TantivyIndex,
        convs: &[NormalizedConversation],
        force_tantivy_reindex: bool,
    ) -> Result<()> {
        if convs.is_empty() {
            return Ok(());
        }

        let begin_concurrent_enabled = begin_concurrent_writes_enabled();
        let duplicate_keys_present =
            begin_concurrent_enabled && duplicate_conversation_keys_present(convs);

        if begin_concurrent_enabled && !duplicate_keys_present {
            let db_path = storage
                .database_path()
                .with_context(|| "resolving database path for begin-concurrent write mode")?;
            tracing::info!(
                conversations = convs.len(),
                "using begin-concurrent write path for indexing"
            );
            return persist_conversations_batched_begin_concurrent(
                &db_path,
                t_index,
                convs,
                force_tantivy_reindex,
            );
        }

        if duplicate_keys_present {
            tracing::info!(
                conversations = convs.len(),
                "duplicate conversation keys detected; falling back to serial batched indexing path"
            );
        }

        let cache_enabled = IndexingCache::is_enabled();
        let mut cache = IndexingCache::new();

        // Prepare data for batched insert: (agent_id, workspace_id, Conversation)
        let mut prepared: Vec<(i64, Option<i64>, Conversation)> = Vec::with_capacity(convs.len());

        for conv in convs {
            let agent = Agent {
                id: None,
                slug: conv.agent_slug.clone(),
                name: conv.agent_slug.clone(),
                version: None,
                kind: AgentKind::Cli,
            };

            let agent_id = if cache_enabled {
                cache.get_or_insert_agent(storage, &agent)?
            } else {
                storage.ensure_agent(&agent)?
            };

            let workspace_id = if let Some(ws) = &conv.workspace {
                if cache_enabled {
                    Some(cache.get_or_insert_workspace(storage, ws, None)?)
                } else {
                    Some(storage.ensure_workspace(ws, None)?)
                }
            } else {
                None
            };

            let internal_conv = map_to_internal(conv);
            prepared.push((agent_id, workspace_id, internal_conv));
        }

        // Log cache statistics if enabled
        if cache_enabled {
            let (hits, misses, hit_rate) = cache.stats();
            tracing::debug!(
                hits,
                misses,
                hit_rate = format!("{:.1}%", hit_rate * 100.0),
                agents = cache.agent_count(),
                workspaces = cache.workspace_count(),
                "IndexingCache stats"
            );
        }

        // Build references for the batched call
        let refs: Vec<(i64, Option<i64>, &Conversation)> =
            prepared.iter().map(|(a, w, c)| (*a, *w, c)).collect();

        // Execute batched insert (single transaction)
        let outcomes = storage.insert_conversations_batched(&refs)?;

        // Add newly inserted messages to Tantivy index
        for (conv, outcome) in convs.iter().zip(outcomes.iter()) {
            if force_tantivy_reindex {
                // Rebuild path: the Tantivy index is known-empty, so index all messages.
                t_index.add_messages(conv, &conv.messages)?;
            } else if !outcome.inserted_indices.is_empty() {
                let new_msgs: Vec<_> = conv
                    .messages
                    .iter()
                    .filter(|m| outcome.inserted_indices.contains(&m.idx))
                    .cloned()
                    .collect();
                t_index.add_messages(conv, &new_msgs)?;
            }
        }

        Ok(())
    }

    fn map_role(role: &str) -> MessageRole {
        match role {
            "user" => MessageRole::User,
            "assistant" | "agent" => MessageRole::Agent,
            "tool" => MessageRole::Tool,
            "system" => MessageRole::System,
            other => MessageRole::Other(other.to_string()),
        }
    }

    #[cfg(test)]
    mod persist_internal_tests {
        use super::*;
        use serial_test::serial;

        struct EnvGuard {
            key: &'static str,
            previous: Option<String>,
        }

        impl Drop for EnvGuard {
            fn drop(&mut self) {
                if let Some(value) = &self.previous {
                    // SAFETY: test helper restores process env key it changed.
                    unsafe {
                        std::env::set_var(self.key, value);
                    }
                } else {
                    // SAFETY: test helper restores process env key it changed.
                    unsafe {
                        std::env::remove_var(self.key);
                    }
                }
            }
        }

        fn set_env(key: &'static str, value: &str) -> EnvGuard {
            let previous = dotenvy::var(key).ok();
            // SAFETY: isolated test mutates a process env var and restores via guard.
            unsafe {
                std::env::set_var(key, value);
            }
            EnvGuard { key, previous }
        }

        #[test]
        fn begin_concurrent_flag_parsing() {
            let _guard = set_env("CASS_INDEXER_BEGIN_CONCURRENT", "1");
            assert!(begin_concurrent_writes_enabled());
        }

        #[test]
        fn begin_concurrent_chunk_size_parsing() {
            let _guard = set_env("CASS_INDEXER_BEGIN_CONCURRENT_CHUNK_SIZE", "7");
            assert_eq!(begin_concurrent_chunk_size(), 7);
        }

        #[test]
        fn begin_concurrent_retry_limit_parsing() {
            let _guard = set_env("CASS_INDEXER_BEGIN_CONCURRENT_RETRIES", "9");
            assert_eq!(begin_concurrent_retry_limit(), 9);
        }

        #[test]
        fn begin_concurrent_writer_cache_parsing() {
            let _guard = set_env("CASS_INDEXER_BEGIN_CONCURRENT_WRITER_CACHE_KIB", "2048");
            assert_eq!(begin_concurrent_writer_cache_kib(), 2048);
        }

        #[test]
        fn begin_concurrent_writer_cache_invalid_defaults() {
            let _guard = set_env("CASS_INDEXER_BEGIN_CONCURRENT_WRITER_CACHE_KIB", "0");
            assert_eq!(begin_concurrent_writer_cache_kib(), 4096);
        }

        #[test]
        fn retryable_franken_errors_are_detected() {
            let retryable = anyhow::Error::new(FrankenError::BusySnapshot {
                conflicting_pages: "1,2".to_string(),
            });
            assert!(is_retryable_franken_error(&retryable));

            let not_retryable = anyhow::Error::new(FrankenError::ConcurrentUnavailable);
            assert!(!is_retryable_franken_error(&not_retryable));
        }

        /// Helper: create a frankensqlite-native database with schema applied.
        fn create_franken_db(path: &std::path::Path) -> FrankenStorage {
            let fs = FrankenStorage::open(path).expect("open frankensqlite db");
            fs.run_migrations().expect("run migrations");
            fs
        }

        #[test]
        fn begin_concurrent_persist_writes_all_conversations() {
            use crate::connectors::{NormalizedConversation, NormalizedMessage};
            use crate::search::tantivy::TantivyIndex;
            use frankensqlite::compat::{ConnectionExt, RowExt};

            let dir = tempfile::TempDir::new().unwrap();
            let db_path = dir.path().join("test.db");
            let index_path = dir.path().join("tantivy");

            // Create frankensqlite-native database (BEGIN CONCURRENT requires it)
            let frank = create_franken_db(&db_path);
            drop(frank); // close so writers can open independently
            let mut t_index = TantivyIndex::open_or_create(&index_path).unwrap();

            // Build 10 conversations across 3 agent slugs
            let convs: Vec<NormalizedConversation> = (0..10)
                .map(|i| {
                    let slug = format!("agent-{}", i % 3);
                    NormalizedConversation {
                        agent_slug: slug,
                        external_id: Some(format!("conv-{i}")),
                        title: Some(format!("Conversation {i}")),
                        workspace: Some(std::path::PathBuf::from(format!("/ws/{i}"))),
                        source_path: std::path::PathBuf::from(format!("/log/{i}.jsonl")),
                        started_at: Some(1000 + i * 100),
                        ended_at: Some(1000 + i * 100 + 50),
                        metadata: serde_json::json!({}),
                        messages: (0..3)
                            .map(|j| NormalizedMessage {
                                idx: j,
                                role: if j % 2 == 0 { "user" } else { "assistant" }.to_string(),
                                author: Some("tester".into()),
                                created_at: Some(1000 + i * 100 + j * 10),
                                content: format!("begin-concurrent-test conv={i} msg={j}"),
                                extra: serde_json::json!({}),
                                snippets: vec![],
                            })
                            .collect(),
                    }
                })
                .collect();

            // Set chunk size < conversation count to exercise multiple parallel writers
            let _chunk_guard = set_env("CASS_INDEXER_BEGIN_CONCURRENT_CHUNK_SIZE", "3");

            persist_conversations_batched_begin_concurrent(&db_path, &mut t_index, &convs, true)
                .expect("begin-concurrent persist should succeed");

            // Verify using FrankenStorage reader
            let reader = FrankenStorage::open(&db_path).unwrap();
            let count: i64 = reader
                .raw()
                .query_row_map("SELECT COUNT(*) FROM conversations", &[], |row| {
                    row.get_typed(0)
                })
                .unwrap();
            assert_eq!(count, 10, "all 10 conversations should be persisted");

            let msg_count: i64 = reader
                .raw()
                .query_row_map("SELECT COUNT(*) FROM messages", &[], |row| row.get_typed(0))
                .unwrap();
            assert_eq!(msg_count, 30, "all 30 messages should be persisted");

            let agent_count: i64 = reader
                .raw()
                .query_row_map("SELECT COUNT(DISTINCT slug) FROM agents", &[], |row| {
                    row.get_typed(0)
                })
                .unwrap();
            assert_eq!(agent_count, 3, "3 distinct agent slugs should exist");

            // Commit tantivy to finalize
            t_index.commit().unwrap();
        }

        #[test]
        fn begin_concurrent_single_conversation_works() {
            use crate::connectors::{NormalizedConversation, NormalizedMessage};
            use crate::search::tantivy::TantivyIndex;
            use frankensqlite::compat::{ConnectionExt, RowExt};

            let dir = tempfile::TempDir::new().unwrap();
            let db_path = dir.path().join("test.db");
            let index_path = dir.path().join("tantivy");

            let frank = create_franken_db(&db_path);
            drop(frank);
            let mut t_index = TantivyIndex::open_or_create(&index_path).unwrap();

            let convs = vec![NormalizedConversation {
                agent_slug: "solo-agent".into(),
                external_id: Some("solo-1".into()),
                title: Some("Solo test".into()),
                workspace: None,
                source_path: std::path::PathBuf::from("/log/solo.jsonl"),
                started_at: Some(5000),
                ended_at: Some(5050),
                metadata: serde_json::json!({}),
                messages: vec![NormalizedMessage {
                    idx: 0,
                    role: "user".into(),
                    author: Some("tester".into()),
                    created_at: Some(5000),
                    content: "single-conv-begin-concurrent-test".into(),
                    extra: serde_json::json!({}),
                    snippets: vec![],
                }],
            }];

            persist_conversations_batched_begin_concurrent(&db_path, &mut t_index, &convs, true)
                .expect("single conversation begin-concurrent persist should succeed");

            let reader = FrankenStorage::open(&db_path).unwrap();
            let count: i64 = reader
                .raw()
                .query_row_map("SELECT COUNT(*) FROM conversations", &[], |row| {
                    row.get_typed(0)
                })
                .unwrap();
            assert_eq!(count, 1);

            let msg_count: i64 = reader
                .raw()
                .query_row_map("SELECT COUNT(*) FROM messages", &[], |row| row.get_typed(0))
                .unwrap();
            assert_eq!(msg_count, 1);
        }

        #[test]
        #[serial]
        fn persist_conversations_batched_falls_back_for_duplicate_keys() {
            use crate::connectors::{NormalizedConversation, NormalizedMessage};
            use crate::search::tantivy::TantivyIndex;
            use crate::sources::provenance::{Source, SourceKind};
            use frankensqlite::compat::{ConnectionExt, RowExt};

            let _begin_guard = set_env("CASS_INDEXER_BEGIN_CONCURRENT", "1");
            let _chunk_guard = set_env("CASS_INDEXER_BEGIN_CONCURRENT_CHUNK_SIZE", "1");

            let dir = tempfile::TempDir::new().unwrap();
            let db_path = dir.path().join("test.db");
            let index_path = dir.path().join("tantivy");

            let storage = create_franken_db(&db_path);
            let mut t_index = TantivyIndex::open_or_create(&index_path).unwrap();
            storage
                .upsert_source(&Source {
                    id: "remote-source".into(),
                    kind: SourceKind::Ssh,
                    host_label: Some("example-host".into()),
                    machine_id: None,
                    platform: None,
                    config_json: None,
                    created_at: None,
                    updated_at: None,
                })
                .unwrap();
            let metadata = serde_json::json!({
                "cass": {
                    "origin": {
                        "source_id": "remote-source",
                        "host": "example-host"
                    }
                }
            });

            let convs = vec![
                NormalizedConversation {
                    agent_slug: "shared-agent".into(),
                    external_id: Some("dup-session".into()),
                    title: Some("Shared Session".into()),
                    workspace: Some(std::path::PathBuf::from("/ws/shared")),
                    source_path: std::path::PathBuf::from("/log/first.jsonl"),
                    started_at: Some(1_000),
                    ended_at: Some(1_010),
                    metadata: metadata.clone(),
                    messages: vec![NormalizedMessage {
                        idx: 2,
                        role: "user".into(),
                        author: Some("tester".into()),
                        created_at: Some(1_002),
                        content: "third".into(),
                        extra: serde_json::json!({}),
                        snippets: vec![],
                    }],
                },
                NormalizedConversation {
                    agent_slug: "shared-agent".into(),
                    external_id: Some("dup-session".into()),
                    title: Some("Shared Session".into()),
                    workspace: Some(std::path::PathBuf::from("/ws/shared")),
                    source_path: std::path::PathBuf::from("/log/second.jsonl"),
                    started_at: Some(1_000),
                    ended_at: Some(1_020),
                    metadata,
                    messages: vec![
                        NormalizedMessage {
                            idx: 0,
                            role: "user".into(),
                            author: Some("tester".into()),
                            created_at: Some(1_000),
                            content: "first".into(),
                            extra: serde_json::json!({}),
                            snippets: vec![],
                        },
                        NormalizedMessage {
                            idx: 1,
                            role: "assistant".into(),
                            author: Some("tester".into()),
                            created_at: Some(1_001),
                            content: "second".into(),
                            extra: serde_json::json!({}),
                            snippets: vec![],
                        },
                    ],
                },
            ];

            persist_conversations_batched(&storage, &mut t_index, &convs, false)
                .expect("duplicate-key batch should fall back to serial path");

            let reader = FrankenStorage::open(&db_path).unwrap();
            let conversation_count: i64 = reader
                .raw()
                .query_row_map("SELECT COUNT(*) FROM conversations", &[], |row| {
                    row.get_typed(0)
                })
                .unwrap();
            assert_eq!(conversation_count, 1);

            let stored_indices: Vec<i64> = reader
                .raw()
                .query_map_collect("SELECT idx FROM messages ORDER BY idx", &[], |row| {
                    row.get_typed(0)
                })
                .unwrap();
            assert_eq!(stored_indices, vec![0, 1, 2]);

            t_index.commit().unwrap();
        }

        #[test]
        fn duplicate_conversation_keys_present_for_shared_source_path_without_external_id() {
            use crate::connectors::{NormalizedConversation, NormalizedMessage};

            let convs = vec![
                NormalizedConversation {
                    agent_slug: "shared-agent".into(),
                    external_id: None,
                    title: Some("Shared Session".into()),
                    workspace: Some(std::path::PathBuf::from("/ws/shared")),
                    source_path: std::path::PathBuf::from("/log/shared.jsonl"),
                    started_at: Some(1_000),
                    ended_at: Some(1_010),
                    metadata: serde_json::json!({}),
                    messages: vec![NormalizedMessage {
                        idx: 0,
                        role: "user".into(),
                        author: Some("tester".into()),
                        created_at: Some(1_000),
                        content: "first".into(),
                        extra: serde_json::json!({}),
                        snippets: vec![],
                    }],
                },
                NormalizedConversation {
                    agent_slug: "shared-agent".into(),
                    external_id: None,
                    title: Some("Shared Session".into()),
                    workspace: Some(std::path::PathBuf::from("/ws/shared")),
                    source_path: std::path::PathBuf::from("/log/shared.jsonl"),
                    started_at: Some(9_999),
                    ended_at: Some(10_010),
                    metadata: serde_json::json!({}),
                    messages: vec![NormalizedMessage {
                        idx: 1,
                        role: "assistant".into(),
                        author: Some("tester".into()),
                        created_at: Some(1_001),
                        content: "second".into(),
                        extra: serde_json::json!({}),
                        snippets: vec![],
                    }],
                },
            ];

            assert!(duplicate_conversation_keys_present(&convs));
        }

        #[test]
        fn begin_concurrent_disabled_falls_through_to_default() {
            let _guard = set_env("CASS_INDEXER_BEGIN_CONCURRENT", "0");
            assert!(!begin_concurrent_writes_enabled());

            let _guard2 = set_env("CASS_INDEXER_BEGIN_CONCURRENT", "false");
            assert!(!begin_concurrent_writes_enabled());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::{
        Connector, DetectionResult, NormalizedConversation, NormalizedMessage,
    };
    use crate::sources::provenance::SourceKind;
    use frankensqlite::compat::{ConnectionExt, ParamValue, RowExt};
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

    fn ignore_sources_config() -> EnvGuard {
        let key = "CASS_IGNORE_SOURCES_CONFIG";
        let previous = dotenvy::var(key).ok();
        // SAFETY: test helper toggles a process-local env var for isolation.
        unsafe {
            std::env::set_var(key, "1");
        }
        EnvGuard { key, previous }
    }

    fn ensure_fts_schema(storage: &FrankenStorage) {
        let count: i64 = storage
            .raw()
            .query_row_map(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'fts_messages'",
                &[] as &[ParamValue],
                |row| row.get_typed(0),
            )
            .unwrap();
        assert_eq!(count, 1, "fts_messages should exist after migrations");
    }

    fn norm_msg(idx: i64, created_at: i64) -> NormalizedMessage {
        NormalizedMessage {
            idx,
            role: "user".into(),
            author: Some("u".into()),
            created_at: Some(created_at),
            content: format!("msg-{idx}"),
            extra: serde_json::json!({}),
            snippets: Vec::new(),
        }
    }

    fn norm_conv(
        external_id: Option<&str>,
        msgs: Vec<NormalizedMessage>,
    ) -> NormalizedConversation {
        NormalizedConversation {
            agent_slug: "tester".into(),
            external_id: external_id.map(std::borrow::ToOwned::to_owned),
            title: Some("Demo".into()),
            workspace: Some(PathBuf::from("/workspace/demo")),
            source_path: PathBuf::from("/logs/demo.jsonl"),
            started_at: msgs.first().and_then(|m| m.created_at),
            ended_at: msgs.last().and_then(|m| m.created_at),
            metadata: serde_json::json!({}),
            messages: msgs,
        }
    }

    struct DetectedRemoteFailureConnector;

    impl Connector for DetectedRemoteFailureConnector {
        fn detect(&self) -> DetectionResult {
            DetectionResult {
                detected: true,
                evidence: vec!["fixture".to_string()],
                root_paths: Vec::new(),
            }
        }

        fn scan(
            &self,
            _ctx: &crate::connectors::ScanContext,
        ) -> anyhow::Result<Vec<NormalizedConversation>> {
            Ok(Vec::new())
        }

        fn scan_with_callback(
            &self,
            ctx: &crate::connectors::ScanContext,
            _on_conversation: &mut dyn FnMut(NormalizedConversation) -> anyhow::Result<()>,
        ) -> anyhow::Result<()> {
            if ctx.scan_roots.is_empty() {
                Ok(())
            } else {
                Err(anyhow::anyhow!("remote exploded"))
            }
        }
    }

    fn detected_remote_failure_connector_factory() -> Box<dyn Connector + Send> {
        Box::new(DetectedRemoteFailureConnector)
    }

    struct PanicConnector;

    impl Connector for PanicConnector {
        fn detect(&self) -> DetectionResult {
            DetectionResult {
                detected: true,
                evidence: vec!["fixture".to_string()],
                root_paths: Vec::new(),
            }
        }

        fn scan(
            &self,
            _ctx: &crate::connectors::ScanContext,
        ) -> anyhow::Result<Vec<NormalizedConversation>> {
            Ok(Vec::new())
        }

        fn scan_with_callback(
            &self,
            _ctx: &crate::connectors::ScanContext,
            _on_conversation: &mut dyn FnMut(NormalizedConversation) -> anyhow::Result<()>,
        ) -> anyhow::Result<()> {
            panic!("connector panic during local scan");
        }
    }

    fn panic_connector_factory() -> Box<dyn Connector + Send> {
        Box::new(PanicConnector)
    }

    static DISCONNECT_TEST_COUNTER: Mutex<Option<Arc<AtomicUsize>>> = Mutex::new(None);

    struct DisconnectAwareConnector;

    impl Connector for DisconnectAwareConnector {
        fn detect(&self) -> DetectionResult {
            DetectionResult {
                detected: true,
                evidence: vec!["fixture".to_string()],
                root_paths: Vec::new(),
            }
        }

        fn scan(
            &self,
            _ctx: &crate::connectors::ScanContext,
        ) -> anyhow::Result<Vec<NormalizedConversation>> {
            Ok(Vec::new())
        }

        fn scan_with_callback(
            &self,
            ctx: &crate::connectors::ScanContext,
            on_conversation: &mut dyn FnMut(NormalizedConversation) -> anyhow::Result<()>,
        ) -> anyhow::Result<()> {
            let counter = DISCONNECT_TEST_COUNTER
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .expect("disconnect test counter should be configured");
            let scope = if ctx.scan_roots.is_empty() {
                "local"
            } else {
                "remote"
            };

            for idx in 0..3 {
                counter.fetch_add(1, Ordering::Relaxed);
                let oversized = NormalizedMessage {
                    content: "x".repeat(DEFAULT_STREAMING_BATCH_LIMITS.max_chars + 1),
                    ..norm_msg(idx, 2_000 + idx)
                };
                on_conversation(norm_conv(Some(scope), vec![oversized]))?;
            }

            Ok(())
        }
    }

    fn disconnect_aware_connector_factory() -> Box<dyn Connector + Send> {
        Box::new(DisconnectAwareConnector)
    }

    #[test]
    fn next_streaming_batch_splits_large_message_batches() {
        let limits = StreamingBatchLimits {
            max_conversations: 8,
            max_messages: 1_000,
            max_chars: usize::MAX,
        };
        let convs = vec![
            norm_conv(
                Some("a"),
                (0..700).map(|i| norm_msg(i, 1_000 + i)).collect(),
            ),
            norm_conv(
                Some("b"),
                (0..400).map(|i| norm_msg(i, 2_000 + i)).collect(),
            ),
            norm_conv(
                Some("c"),
                (0..300).map(|i| norm_msg(i, 3_000 + i)).collect(),
            ),
        ];

        let mut iter = convs.into_iter().peekable();
        let (batch1, batch1_messages) = next_streaming_batch(&mut iter, limits).unwrap();
        let (batch2, batch2_messages) = next_streaming_batch(&mut iter, limits).unwrap();

        assert_eq!(
            batch1
                .iter()
                .map(|conv| conv.external_id.as_deref().unwrap())
                .collect::<Vec<_>>(),
            vec!["a"]
        );
        assert_eq!(batch1_messages, 700);

        assert_eq!(
            batch2
                .iter()
                .map(|conv| conv.external_id.as_deref().unwrap())
                .collect::<Vec<_>>(),
            vec!["b", "c"]
        );
        assert_eq!(batch2_messages, 700);
        assert!(next_streaming_batch(&mut iter, limits).is_none());
    }

    #[test]
    fn next_streaming_batch_keeps_single_oversized_conversation_isolated() {
        let limits = StreamingBatchLimits {
            max_conversations: 8,
            max_messages: 8,
            max_chars: 64,
        };
        let oversized = NormalizedMessage {
            content: "x".repeat(256),
            ..norm_msg(0, 1_000)
        };
        let convs = vec![
            norm_conv(Some("huge"), vec![oversized]),
            norm_conv(Some("small"), vec![norm_msg(0, 2_000)]),
        ];

        let mut iter = convs.into_iter().peekable();
        let (batch1, batch1_messages) = next_streaming_batch(&mut iter, limits).unwrap();
        let (batch2, batch2_messages) = next_streaming_batch(&mut iter, limits).unwrap();

        assert_eq!(
            batch1[0].external_id.as_deref(),
            Some("huge"),
            "oversized conversations should still index, but alone"
        );
        assert_eq!(batch1_messages, 1);
        assert_eq!(batch2[0].external_id.as_deref(), Some("small"));
        assert_eq!(batch2_messages, 1);
        assert!(next_streaming_batch(&mut iter, limits).is_none());
    }

    #[test]
    fn streaming_batch_sender_flushes_single_oversized_conversation_immediately() {
        let (tx, rx) = bounded(2);
        let mut sender = StreamingBatchSender::new(
            &tx,
            Arc::new(StreamingByteLimiter::new(STREAMING_MAX_BYTES_IN_FLIGHT)),
            "gemini",
            false,
        );
        let oversized = NormalizedMessage {
            content: "x".repeat(DEFAULT_STREAMING_BATCH_LIMITS.max_chars + 1),
            ..norm_msg(0, 1_000)
        };
        let conversation = norm_conv(Some("huge"), vec![oversized]);

        sender
            .push(conversation)
            .expect("oversized conversation should still flush even in tests");

        match rx
            .try_recv()
            .expect("oversized conversation should flush immediately")
        {
            IndexMessage::Batch {
                connector_name,
                conversations,
                message_count,
                byte_reservation,
                ..
            } => {
                assert_eq!(connector_name, "gemini");
                assert_eq!(conversations.len(), 1);
                assert_eq!(conversations[0].external_id.as_deref(), Some("huge"));
                assert_eq!(message_count, 1);
                assert_eq!(
                    byte_reservation,
                    DEFAULT_STREAMING_BATCH_LIMITS.max_chars + 1
                );
            }
            other => panic!(
                "expected batch for oversized conversation flush, got {:?}",
                std::mem::discriminant(&other)
            ),
        }

        assert!(
            rx.try_recv().is_err(),
            "sender buffer should be empty after auto-flush"
        );
        sender.flush().unwrap();
        assert!(rx.try_recv().is_err(), "explicit flush should be a no-op");
    }

    #[test]
    fn streaming_byte_limiter_blocks_until_capacity_is_released() {
        let limiter = Arc::new(StreamingByteLimiter::new(64));
        let first = limiter.acquire(128).unwrap();
        let (ready_tx, ready_rx) = bounded(1);
        let (result_tx, result_rx) = bounded(1);
        let waiter = {
            let limiter = limiter.clone();
            thread::spawn(move || {
                ready_tx.send(()).unwrap();
                let second = limiter.acquire(32).unwrap();
                result_tx.send(second).unwrap();
                limiter.release(second);
            })
        };

        ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(
            result_rx.try_recv().is_err(),
            "waiter should remain blocked while the limiter is full"
        );

        limiter.release(first);
        assert_eq!(result_rx.recv_timeout(Duration::from_secs(1)).unwrap(), 32);
        waiter.join().unwrap();
    }

    #[test]
    fn streaming_byte_limiter_close_wakes_waiters() {
        let limiter = Arc::new(StreamingByteLimiter::new(64));
        let first = limiter.acquire(64).unwrap();
        let (ready_tx, ready_rx) = bounded(1);
        let (result_tx, result_rx) = bounded(1);
        let waiter = {
            let limiter = limiter.clone();
            thread::spawn(move || {
                ready_tx.send(()).unwrap();
                let result = limiter.acquire(1).map_err(|error| error.to_string());
                result_tx.send(result).unwrap();
            })
        };

        ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(
            result_rx.try_recv().is_err(),
            "waiter should remain blocked until the limiter is closed"
        );

        limiter.close();
        let error = result_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .expect_err("closing the limiter should wake blocked waiters with an error");
        assert!(error.contains("closed"));
        limiter.release(first);
        waiter.join().unwrap();
    }

    #[test]
    fn send_conversation_batches_marks_only_first_batch_as_discovered() {
        let (tx, rx) = bounded(4);
        let convs = vec![
            norm_conv(
                Some("a"),
                (0..1_200).map(|i| norm_msg(i, 1_000 + i)).collect(),
            ),
            norm_conv(
                Some("b"),
                (0..1_200).map(|i| norm_msg(i, 2_000 + i)).collect(),
            ),
        ];

        send_conversation_batches(&tx, "claude", convs, true);
        drop(tx);

        let batches = rx.try_iter().collect::<Vec<_>>();
        assert_eq!(batches.len(), 2);

        match &batches[0] {
            IndexMessage::Batch {
                connector_name,
                is_discovered,
                message_count,
                conversations,
                ..
            } => {
                assert_eq!(*connector_name, "claude");
                assert!(*is_discovered);
                assert_eq!(*message_count, 1_200);
                assert_eq!(conversations.len(), 1);
            }
            _ => panic!("expected first message to be a batch"),
        }

        match &batches[1] {
            IndexMessage::Batch {
                connector_name,
                is_discovered,
                message_count,
                conversations,
                ..
            } => {
                assert_eq!(*connector_name, "claude");
                assert!(!*is_discovered);
                assert_eq!(*message_count, 1_200);
                assert_eq!(conversations.len(), 1);
            }
            _ => panic!("expected second message to be a batch"),
        }
    }

    #[test]
    fn streaming_consumer_preserves_discovered_connector_with_no_batches() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let db_path = data_dir.join("db.sqlite");
        let storage = FrankenStorage::open(&db_path).unwrap();
        ensure_fts_schema(&storage);
        let mut index = TantivyIndex::open_or_create(&index_dir(&data_dir).unwrap()).unwrap();
        let progress = Arc::new(IndexingProgress::default());
        let (tx, rx) = bounded(2);

        tx.send(IndexMessage::Done {
            connector_name: "claude",
            scan_ms: 42,
            is_discovered: true,
        })
        .unwrap();
        drop(tx);

        let discovered = run_streaming_consumer(
            rx,
            1,
            &storage,
            &mut index,
            Arc::new(StreamingByteLimiter::new(STREAMING_MAX_BYTES_IN_FLIGHT)),
            &Some(progress.clone()),
            false,
        )
        .unwrap();

        assert_eq!(discovered, vec!["claude".to_string()]);
        let stats = progress.stats.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(stats.agents_discovered, vec!["claude".to_string()]);
        assert_eq!(stats.total_conversations, 0);
        assert_eq!(stats.total_messages, 0);
    }

    #[test]
    fn streaming_producer_records_remote_scan_errors_in_connector_stats() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let db_path = data_dir.join("db.sqlite");
        let storage = FrankenStorage::open(&db_path).unwrap();
        ensure_fts_schema(&storage);
        let mut index = TantivyIndex::open_or_create(&index_dir(&data_dir).unwrap()).unwrap();
        let progress = Arc::new(IndexingProgress::default());
        let (tx, rx) = bounded(STREAMING_CHANNEL_SIZE);
        let flow_limiter = Arc::new(StreamingByteLimiter::new(STREAMING_MAX_BYTES_IN_FLIGHT));
        let remote_root_path = PathBuf::from("/remote/fixture/claude");
        let handle = spawn_connector_producer(
            "claude",
            detected_remote_failure_connector_factory,
            tx,
            StreamingProducerConfig {
                flow_limiter: flow_limiter.clone(),
                data_dir,
                remote_roots: vec![ScanRoot::remote(
                    remote_root_path.clone(),
                    Origin::remote("fixture-host"),
                    Some(crate::sources::config::Platform::Linux),
                )],
                since_ts: None,
                progress: Some(progress.clone()),
            },
        );

        let discovered = run_streaming_consumer(
            rx,
            1,
            &storage,
            &mut index,
            flow_limiter,
            &Some(progress.clone()),
            false,
        )
        .unwrap();
        handle.join().unwrap();

        assert_eq!(discovered, vec!["claude".to_string()]);

        let stats = progress.stats.lock().unwrap_or_else(|e| e.into_inner());
        let connector = stats
            .connectors
            .iter()
            .find(|connector| connector.name == "claude")
            .expect("claude connector stats should exist");
        assert_eq!(
            connector.error.as_deref(),
            Some("remote scan failed for /remote/fixture/claude: remote exploded")
        );
    }

    #[test]
    fn streaming_index_fails_closed_when_producer_panics() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let db_path = data_dir.join("db.sqlite");
        let storage = FrankenStorage::open(&db_path).unwrap();
        ensure_fts_schema(&storage);
        let mut index = TantivyIndex::open_or_create(&index_dir(&data_dir).unwrap()).unwrap();
        let progress = Arc::new(IndexingProgress::default());
        let opts = IndexOptions {
            full: false,
            force_rebuild: false,
            watch: false,
            watch_once_paths: None,
            db_path,
            data_dir,
            semantic: false,
            build_hnsw: false,
            embedder: "fastembed".to_string(),
            progress: Some(progress.clone()),
            watch_interval_secs: 30,
        };

        let error = run_streaming_index_with_connector_factories(
            &storage,
            &mut index,
            &opts,
            None,
            false,
            Vec::new(),
            vec![("claude", panic_connector_factory)],
        )
        .expect_err("producer panic should abort streaming indexing");
        let message = error.to_string();
        assert!(
            message.contains("streaming producer thread panicked"),
            "panic should surface in the returned error: {message}"
        );
        assert!(
            message.contains("claude: connector panic during local scan"),
            "returned error should name the failing connector and panic: {message}"
        );
        assert_eq!(
            progress
                .last_error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_deref(),
            Some(message.as_str()),
            "progress tracker should expose the real panic instead of pretending indexing succeeded"
        );
    }

    #[test]
    fn streaming_producer_stops_after_consumer_disconnect() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let counter = Arc::new(AtomicUsize::new(0));
        *DISCONNECT_TEST_COUNTER
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(counter.clone());

        let (tx, rx) = bounded(STREAMING_CHANNEL_SIZE);
        drop(rx);

        let handle = spawn_connector_producer(
            "claude",
            disconnect_aware_connector_factory,
            tx,
            StreamingProducerConfig {
                flow_limiter: Arc::new(StreamingByteLimiter::new(STREAMING_MAX_BYTES_IN_FLIGHT)),
                data_dir,
                remote_roots: vec![ScanRoot::remote(
                    PathBuf::from("/remote/fixture/claude"),
                    Origin::remote("fixture-host"),
                    Some(crate::sources::config::Platform::Linux),
                )],
                since_ts: None,
                progress: None,
            },
        );

        handle
            .join()
            .expect("producer should stop cleanly after consumer disconnect");
        assert_eq!(
            counter.load(Ordering::Relaxed),
            1,
            "producer should stop after the first failed batch send instead of chewing through local and remote scans"
        );

        *DISCONNECT_TEST_COUNTER
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
    }

    #[test]
    fn db_id_conversion_helpers_handle_invalid_ranges() {
        assert_eq!(message_id_from_db(-1), None);
        assert_eq!(message_id_from_db(0), Some(0));
        assert_eq!(message_id_from_db(42), Some(42));

        assert_eq!(saturating_u32_from_i64(-9), 0);
        assert_eq!(saturating_u32_from_i64(17), 17);
        assert_eq!(
            saturating_u32_from_i64(i64::from(u32::MAX) + 1234),
            u32::MAX
        );
    }

    #[test]
    fn open_storage_for_index_recovers_from_newer_schema() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("future-schema.db");

        {
            let storage = FrankenStorage::open(&db_path).unwrap();
            storage
                .raw()
                .execute_compat(
                    "INSERT OR REPLACE INTO meta(key, value) VALUES('schema_version', ?1)",
                    &[ParamValue::from(format!(
                        "{}",
                        crate::storage::sqlite::CURRENT_SCHEMA_VERSION + 1
                    ))],
                )
                .unwrap();
        }

        let (storage, rebuilt, opened_fresh_for_full) =
            open_storage_for_index(&db_path, false).unwrap();
        assert!(rebuilt, "newer schema should trigger rebuild recovery");
        assert!(opened_fresh_for_full);
        assert_eq!(
            storage.schema_version().unwrap(),
            crate::storage::sqlite::CURRENT_SCHEMA_VERSION
        );

        // Rebuild path should preserve an on-disk backup.
        let backup_count = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .map(|name| name.starts_with("future-schema.db.backup."))
                    .unwrap_or(false)
            })
            .count();
        assert!(
            backup_count >= 1,
            "expected backup artifact for rebuilt schema"
        );
    }

    #[test]
    fn reset_storage_clears_data_but_leaves_meta() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("db.sqlite");
        let storage = FrankenStorage::open(&db_path).unwrap();
        ensure_fts_schema(&storage);

        let agent = crate::model::types::Agent {
            id: None,
            slug: "tester".into(),
            name: "Tester".into(),
            version: None,
            kind: crate::model::types::AgentKind::Cli,
        };
        let agent_id = storage.ensure_agent(&agent).unwrap();
        let conv = norm_conv(Some("c1"), vec![norm_msg(0, 10)]);
        storage
            .insert_conversation_tree(
                agent_id,
                None,
                &crate::model::types::Conversation {
                    id: None,
                    agent_slug: conv.agent_slug.clone(),
                    workspace: conv.workspace.clone(),
                    external_id: conv.external_id.clone(),
                    title: conv.title.clone(),
                    source_path: conv.source_path.clone(),
                    started_at: conv.started_at,
                    ended_at: conv.ended_at,
                    approx_tokens: None,
                    metadata_json: conv.metadata.clone(),
                    messages: conv
                        .messages
                        .iter()
                        .map(|m| crate::model::types::Message {
                            id: None,
                            idx: m.idx,
                            role: crate::model::types::MessageRole::User,
                            author: m.author.clone(),
                            created_at: m.created_at,
                            content: m.content.clone(),
                            extra_json: m.extra.clone(),
                            snippets: Vec::new(),
                        })
                        .collect(),
                    source_id: "local".to_string(),
                    origin_host: None,
                },
            )
            .unwrap();

        let msg_count: i64 = storage
            .raw()
            .query_row_map("SELECT COUNT(*) FROM messages", &[] as &[ParamValue], |r| {
                r.get_typed(0)
            })
            .unwrap();
        assert_eq!(msg_count, 1);

        storage
            .raw()
            .execute_compat(
                "INSERT INTO daily_stats(day_id, agent_slug, source_id, session_count, message_count, total_chars, last_updated)
                 VALUES(?1, ?2, ?3, 1, 1, 10, ?4)",
                &[
                    ParamValue::from(1_i64),
                    ParamValue::from("tester"),
                    ParamValue::from("local"),
                    ParamValue::from(123_i64),
                ],
            )
            .unwrap();
        storage
            .raw()
            .execute_compat(
                "INSERT INTO usage_daily(day_id, agent_slug, workspace_id, source_id, message_count, last_updated)
                 VALUES(?1, ?2, ?3, ?4, 1, ?5)",
                &[
                    ParamValue::from(1_i64),
                    ParamValue::from("tester"),
                    ParamValue::from(0_i64),
                    ParamValue::from("local"),
                    ParamValue::from(123_i64),
                ],
            )
            .unwrap();

        reset_storage(&storage, &db_path).unwrap();

        let msg_count: i64 = storage
            .raw()
            .query_row_map("SELECT COUNT(*) FROM messages", &[] as &[ParamValue], |r| {
                r.get_typed(0)
            })
            .unwrap();
        assert_eq!(msg_count, 0);
        let daily_count: i64 = storage
            .raw()
            .query_row_map(
                "SELECT COUNT(*) FROM daily_stats",
                &[] as &[ParamValue],
                |r| r.get_typed(0),
            )
            .unwrap();
        assert_eq!(daily_count, 0);
        let usage_daily_count: i64 = storage
            .raw()
            .query_row_map(
                "SELECT COUNT(*) FROM usage_daily",
                &[] as &[ParamValue],
                |r| r.get_typed(0),
            )
            .unwrap();
        assert_eq!(usage_daily_count, 0);
        let fts_count: i64 = storage
            .raw()
            .query_row_map(
                "SELECT COUNT(*) FROM fts_messages",
                &[] as &[ParamValue],
                |r| r.get_typed(0),
            )
            .unwrap();
        assert_eq!(fts_count, 0, "reset should recreate an empty FTS table");
        assert_eq!(
            storage.schema_version().unwrap(),
            crate::storage::sqlite::CURRENT_SCHEMA_VERSION
        );
    }

    #[test]
    fn repair_daily_stats_if_drifted_rebuilds_materialized_totals() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("db.sqlite");
        let storage = FrankenStorage::open(&db_path).unwrap();
        storage.run_migrations().unwrap();

        let agent = crate::model::types::Agent {
            id: None,
            slug: "tester".into(),
            name: "Tester".into(),
            version: None,
            kind: crate::model::types::AgentKind::Cli,
        };
        let agent_id = storage.ensure_agent(&agent).unwrap();
        let conversation = crate::model::types::Conversation {
            id: None,
            agent_slug: "tester".into(),
            workspace: Some(std::path::PathBuf::from("/tmp/workspace")),
            external_id: Some("daily-stats-repair".into()),
            title: Some("repair".into()),
            source_path: std::path::PathBuf::from("/tmp/repair.jsonl"),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_000_100),
            approx_tokens: None,
            metadata_json: serde_json::Value::Null,
            messages: vec![crate::model::types::Message {
                id: None,
                idx: 0,
                role: crate::model::types::MessageRole::User,
                author: None,
                created_at: Some(1_700_000_000_000),
                content: "hello".into(),
                extra_json: serde_json::Value::Null,
                snippets: Vec::new(),
            }],
            source_id: "local".into(),
            origin_host: None,
        };
        storage
            .insert_conversations_batched(&[(agent_id, None, &conversation)])
            .unwrap();

        storage.raw().execute("DELETE FROM daily_stats").unwrap();
        storage
            .raw()
            .execute(
                "INSERT INTO daily_stats(day_id, agent_slug, source_id, session_count, message_count, total_chars, last_updated)
                 VALUES(0, 'all', 'all', 99, 99, 99, 0)",
            )
            .unwrap();

        let before = storage.daily_stats_health().unwrap();
        assert_eq!(before.materialized_total, 99);
        assert!(before.drift > 0);

        repair_daily_stats_if_drifted(&storage, &db_path).unwrap();
        let after = storage.daily_stats_health().unwrap();
        assert_eq!(after.conversation_count, 1);
        assert_eq!(after.materialized_total, 1);
        assert_eq!(after.drift, 0);
    }

    #[test]
    fn historical_salvage_decision_skips_populated_canonical_db() {
        assert!(!should_salvage_historical_databases(false, 1, false, false));
        assert!(!should_salvage_historical_databases(
            false, 43_678, false, false
        ));
    }

    #[test]
    fn historical_salvage_decision_keeps_empty_or_rebuilt_storage() {
        assert!(should_salvage_historical_databases(false, 0, false, false));
        assert!(should_salvage_historical_databases(true, 0, false, false));
        assert!(should_salvage_historical_databases(
            true, 43_678, false, false
        ));
    }

    #[test]
    fn historical_salvage_decision_keeps_populated_canonical_when_more_bundles_are_pending() {
        assert!(should_salvage_historical_databases(
            false, 43_678, true, false
        ));
    }

    #[test]
    fn historical_salvage_decision_skips_pending_bundles_during_canonical_only_full_rebuild() {
        assert!(!should_salvage_historical_databases(
            false, 43_678, true, true
        ));
    }

    #[test]
    fn full_rebuild_does_not_restart_based_on_historical_local_rowids() {
        fn insert_demo_conversation(db_path: &Path, external_id: &str, msg_idx: i64, ts: i64) {
            let storage = crate::storage::sqlite::SqliteStorage::open(db_path).unwrap();
            let agent = crate::model::types::Agent {
                id: None,
                slug: "tester".into(),
                name: "Tester".into(),
                version: None,
                kind: crate::model::types::AgentKind::Cli,
            };
            let agent_id = storage.ensure_agent(&agent).unwrap();
            let conv = norm_conv(Some(external_id), vec![norm_msg(msg_idx, ts)]);
            storage
                .insert_conversation_tree(
                    agent_id,
                    None,
                    &crate::model::types::Conversation {
                        id: None,
                        agent_slug: conv.agent_slug.clone(),
                        workspace: conv.workspace.clone(),
                        external_id: conv.external_id.clone(),
                        title: conv.title.clone(),
                        source_path: conv.source_path.clone(),
                        started_at: conv.started_at,
                        ended_at: conv.ended_at,
                        approx_tokens: None,
                        metadata_json: conv.metadata.clone(),
                        messages: conv
                            .messages
                            .iter()
                            .map(|m| crate::model::types::Message {
                                id: None,
                                idx: m.idx,
                                role: crate::model::types::MessageRole::User,
                                author: m.author.clone(),
                                created_at: m.created_at,
                                content: m.content.clone(),
                                extra_json: m.extra.clone(),
                                snippets: Vec::new(),
                            })
                            .collect(),
                        source_id: "local".to_string(),
                        origin_host: None,
                    },
                )
                .unwrap();
            drop(storage);
            crate::storage::sqlite::rebuild_fts_via_rusqlite(db_path).unwrap();
        }

        let tmp = TempDir::new().unwrap();
        let canonical_db = tmp.path().join("agent_search.db");
        let backups_dir = tmp.path().join("backups");
        std::fs::create_dir_all(&backups_dir).unwrap();
        let healthy_backup = backups_dir.join("agent_search.db.20260322T020200.bak");

        insert_demo_conversation(&canonical_db, "canonical-only", 0, 1_700_000_000_000);
        insert_demo_conversation(&healthy_backup, "backup-1", 0, 1_700_000_000_100);
        insert_demo_conversation(&healthy_backup, "backup-2", 1, 1_700_000_000_200);

        let conn = rusqlite::Connection::open(&canonical_db).unwrap();
        conn.execute(
            "INSERT INTO meta(key, value) VALUES(?1, ?2)",
            rusqlite::params![
                "historical_bundle_salvaged:test",
                "{\"salvage_version\":2,\"method\":\"baseline-bulk-sql-copy\"}"
            ],
        )
        .unwrap();
        drop(conn);

        let storage = FrankenStorage::open(&canonical_db).unwrap();
        let canonical_sessions = count_total_conversations_exact(&storage).unwrap();
        assert_eq!(canonical_sessions, 1);

        assert!(
            !full_rebuild_requires_historical_restart(&storage, &canonical_db, canonical_sessions)
                .unwrap(),
            "full rebuild must not compare local message rowids across different sqlite files"
        );
    }

    #[test]
    fn full_rebuild_restart_ignores_stale_progress_when_canonical_is_healthy() {
        fn insert_demo_conversation(db_path: &Path, external_id: &str, msg_idx: i64, ts: i64) {
            let storage = crate::storage::sqlite::SqliteStorage::open(db_path).unwrap();
            let agent = crate::model::types::Agent {
                id: None,
                slug: "tester".into(),
                name: "Tester".into(),
                version: None,
                kind: crate::model::types::AgentKind::Cli,
            };
            let agent_id = storage.ensure_agent(&agent).unwrap();
            let conv = norm_conv(Some(external_id), vec![norm_msg(msg_idx, ts)]);
            storage
                .insert_conversation_tree(
                    agent_id,
                    None,
                    &crate::model::types::Conversation {
                        id: None,
                        agent_slug: conv.agent_slug.clone(),
                        workspace: conv.workspace.clone(),
                        external_id: conv.external_id.clone(),
                        title: conv.title.clone(),
                        source_path: conv.source_path.clone(),
                        started_at: conv.started_at,
                        ended_at: conv.ended_at,
                        approx_tokens: None,
                        metadata_json: conv.metadata.clone(),
                        messages: conv
                            .messages
                            .iter()
                            .map(|m| crate::model::types::Message {
                                id: None,
                                idx: m.idx,
                                role: crate::model::types::MessageRole::User,
                                author: m.author.clone(),
                                created_at: m.created_at,
                                content: m.content.clone(),
                                extra_json: m.extra.clone(),
                                snippets: Vec::new(),
                            })
                            .collect(),
                        source_id: "local".to_string(),
                        origin_host: None,
                    },
                )
                .unwrap();
            drop(storage);
            crate::storage::sqlite::rebuild_fts_via_rusqlite(db_path).unwrap();
        }

        let tmp = TempDir::new().unwrap();
        let canonical_db = tmp.path().join("agent_search.db");
        let backups_dir = tmp.path().join("backups");
        std::fs::create_dir_all(&backups_dir).unwrap();
        let healthy_backup = backups_dir.join("agent_search.db.20260322T020200.bak");

        insert_demo_conversation(&canonical_db, "canonical-only", 0, 1_700_000_000_000);
        insert_demo_conversation(&healthy_backup, "backup-only", 0, 1_700_000_000_100);

        let storage = FrankenStorage::open(&canonical_db).unwrap();
        storage
            .raw()
            .execute_compat(
                "INSERT INTO meta(key, value) VALUES(?1, ?2)",
                &[
                    ParamValue::from("historical_bundle_progress:test"),
                    ParamValue::from(
                        "{\"progress_version\":1,\"last_completed_source_row_id\":78}",
                    ),
                ],
            )
            .unwrap();

        let canonical_sessions = count_total_conversations_exact(&storage).unwrap();
        assert_eq!(canonical_sessions, 1);
        assert!(
            !full_rebuild_requires_historical_restart(&storage, &canonical_db, canonical_sessions)
                .unwrap(),
            "stale salvage progress alone must not force a fresh canonical restart when the canonical db is healthy"
        );
    }

    #[test]
    fn reopen_fresh_storage_for_full_rebuild_preserves_backup_and_starts_empty() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("db.sqlite");
        let storage = FrankenStorage::open(&db_path).unwrap();
        ensure_fts_schema(&storage);

        let agent = crate::model::types::Agent {
            id: None,
            slug: "tester".into(),
            name: "Tester".into(),
            version: None,
            kind: crate::model::types::AgentKind::Cli,
        };
        let agent_id = storage.ensure_agent(&agent).unwrap();
        let conv = norm_conv(Some("c1"), vec![norm_msg(0, 10)]);
        storage
            .insert_conversation_tree(
                agent_id,
                None,
                &crate::model::types::Conversation {
                    id: None,
                    agent_slug: conv.agent_slug.clone(),
                    workspace: conv.workspace.clone(),
                    external_id: conv.external_id.clone(),
                    title: conv.title.clone(),
                    source_path: conv.source_path.clone(),
                    started_at: conv.started_at,
                    ended_at: conv.ended_at,
                    approx_tokens: None,
                    metadata_json: conv.metadata.clone(),
                    messages: conv
                        .messages
                        .iter()
                        .map(|m| crate::model::types::Message {
                            id: None,
                            idx: m.idx,
                            role: crate::model::types::MessageRole::User,
                            author: m.author.clone(),
                            created_at: m.created_at,
                            content: m.content.clone(),
                            extra_json: m.extra.clone(),
                            snippets: Vec::new(),
                        })
                        .collect(),
                    source_id: "local".to_string(),
                    origin_host: None,
                },
            )
            .unwrap();

        let reopened = reopen_fresh_storage_for_full_rebuild(storage, &db_path).unwrap();
        let msg_count: i64 = reopened
            .raw()
            .query_row_map("SELECT COUNT(*) FROM messages", &[] as &[ParamValue], |r| {
                r.get_typed(0)
            })
            .unwrap();
        assert_eq!(msg_count, 0);

        let backup_count = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .map(|name| name.starts_with("db.sqlite.backup."))
                    .unwrap_or(false)
            })
            .count();
        assert!(
            backup_count >= 1,
            "expected preserved backup before opening a fresh full-rebuild db"
        );
    }

    #[test]
    fn persist_append_only_adds_new_messages_to_index() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let db_path = data_dir.join("db.sqlite");
        let storage = FrankenStorage::open(&db_path).unwrap();
        ensure_fts_schema(&storage);
        let mut index = TantivyIndex::open_or_create(&index_dir(&data_dir).unwrap()).unwrap();

        let conv1 = norm_conv(Some("ext"), vec![norm_msg(0, 100), norm_msg(1, 200)]);
        persist::persist_conversation(&storage, &mut index, &conv1).unwrap();
        index.commit().unwrap();

        let reader = index.reader().unwrap();
        reader.reload().unwrap();
        assert_eq!(reader.searcher().num_docs(), 2);

        let conv2 = norm_conv(
            Some("ext"),
            vec![norm_msg(0, 100), norm_msg(1, 200), norm_msg(2, 300)],
        );
        persist::persist_conversation(&storage, &mut index, &conv2).unwrap();
        index.commit().unwrap();

        let reader = index.reader().unwrap();
        reader.reload().unwrap();
        assert_eq!(reader.searcher().num_docs(), 3);
    }

    #[test]
    fn classify_paths_uses_latest_mtime_per_connector() {
        let tmp = TempDir::new().unwrap();
        let codex = tmp.path().join(".codex/sessions/rollout-1.jsonl");
        std::fs::create_dir_all(codex.parent().unwrap()).unwrap();
        std::fs::write(&codex, "{{}}\n{{}}").unwrap();

        let claude = tmp.path().join("project/.claude.json");
        std::fs::create_dir_all(claude.parent().unwrap()).unwrap();
        std::fs::write(&claude, "{{}}").unwrap();

        let aider = tmp.path().join("repo/.aider.chat.history.md");
        std::fs::create_dir_all(aider.parent().unwrap()).unwrap();
        std::fs::write(&aider, "user\nassistant").unwrap();

        let cursor = tmp.path().join("Cursor/User/globalStorage/state.vscdb");
        std::fs::create_dir_all(cursor.parent().unwrap()).unwrap();
        std::fs::write(&cursor, b"").unwrap();

        let chatgpt = tmp
            .path()
            .join("Library/Application Support/com.openai.chat/conversations-abc/data.json");
        std::fs::create_dir_all(chatgpt.parent().unwrap()).unwrap();
        std::fs::write(&chatgpt, "{}").unwrap();

        // roots are needed for classify_paths now
        let roots = vec![
            (
                ConnectorKind::Codex,
                ScanRoot::local(tmp.path().join(".codex")),
            ),
            (
                ConnectorKind::Claude,
                ScanRoot::local(tmp.path().join("project")),
            ),
            (
                ConnectorKind::Aider,
                ScanRoot::local(tmp.path().join("repo")),
            ),
            (
                ConnectorKind::Cursor,
                ScanRoot::local(tmp.path().join("Cursor/User")),
            ),
            (
                ConnectorKind::ChatGpt,
                ScanRoot::local(
                    tmp.path()
                        .join("Library/Application Support/com.openai.chat"),
                ),
            ),
        ];

        let paths = vec![codex.clone(), claude.clone(), aider, cursor, chatgpt];
        let classified = classify_paths(paths, &roots);

        let kinds: std::collections::HashSet<_> =
            classified.iter().map(|(k, _, _, _)| *k).collect();
        assert!(kinds.contains(&ConnectorKind::Codex));
        assert!(kinds.contains(&ConnectorKind::Claude));
        assert!(kinds.contains(&ConnectorKind::Aider));
        assert!(kinds.contains(&ConnectorKind::Cursor));
        assert!(kinds.contains(&ConnectorKind::ChatGpt));

        for (_, _, mtime, _) in classified {
            assert!(mtime.is_some(), "mtime should be captured");
        }
    }

    #[test]
    fn watch_event_filter_ignores_read_access_noise() {
        let event = notify::Event::new(notify::event::EventKind::Access(AccessKind::Read))
            .add_path(PathBuf::from("/tmp/session.jsonl"));
        assert!(
            !watch_event_should_trigger_reindex(&event),
            "read-only access events should not retrigger watch indexing"
        );

        let event = notify::Event::new(notify::event::EventKind::Access(AccessKind::Close(
            AccessMode::Read,
        )))
        .add_path(PathBuf::from("/tmp/session.jsonl"));
        assert!(
            !watch_event_should_trigger_reindex(&event),
            "close-after-read events should not retrigger watch indexing"
        );
    }

    #[test]
    fn watch_event_filter_keeps_mutating_events() {
        let event = notify::Event::new(notify::event::EventKind::Access(AccessKind::Close(
            AccessMode::Write,
        )))
        .add_path(PathBuf::from("/tmp/session.jsonl"));
        assert!(
            watch_event_should_trigger_reindex(&event),
            "close-after-write events should still retrigger indexing"
        );

        let event = notify::Event::new(notify::event::EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::WriteTime,
        )))
        .add_path(PathBuf::from("/tmp/session.jsonl"));
        assert!(
            watch_event_should_trigger_reindex(&event),
            "write-time metadata changes should still retrigger indexing"
        );
    }

    #[test]
    fn watch_event_filter_ignores_access_time_metadata() {
        let event = notify::Event::new(notify::event::EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::AccessTime,
        )))
        .add_path(PathBuf::from("/tmp/session.jsonl"));
        assert!(
            !watch_event_should_trigger_reindex(&event),
            "access-time metadata changes are read noise and should be ignored"
        );
    }

    #[test]
    fn watch_event_filter_ignores_remove_events_without_delete_support() {
        let event = notify::Event::new(notify::event::EventKind::Remove(
            notify::event::RemoveKind::File,
        ))
        .add_path(PathBuf::from("/tmp/session.jsonl"));
        assert!(
            !watch_event_should_trigger_reindex(&event),
            "remove events should be ignored until watch mode can remove stale indexed rows"
        );
    }

    #[test]
    #[serial]
    fn watch_state_round_trips_to_disk() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let mut state = HashMap::new();
        state.insert(ConnectorKind::Codex, 123);
        state.insert(ConnectorKind::Gemini, 456);

        save_watch_state(&data_dir, &state).unwrap();

        let loaded = load_watch_state(&data_dir);
        assert_eq!(loaded.get(&ConnectorKind::Codex), Some(&123));
        assert_eq!(loaded.get(&ConnectorKind::Gemini), Some(&456));
    }

    #[test]
    #[serial]
    fn watch_state_overwrites_existing_file() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let mut first = HashMap::new();
        first.insert(ConnectorKind::Codex, 111);
        save_watch_state(&data_dir, &first).unwrap();

        let mut second = HashMap::new();
        second.insert(ConnectorKind::Amp, 222);
        save_watch_state(&data_dir, &second).unwrap();

        let loaded = load_watch_state(&data_dir);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded.get(&ConnectorKind::Amp), Some(&222));
        assert!(!loaded.contains_key(&ConnectorKind::Codex));
    }

    #[test]
    fn watch_state_temp_paths_are_unique() {
        let final_path = Path::new("/tmp/watch_state.json");
        let first = unique_atomic_temp_path(final_path);
        let second = unique_atomic_temp_path(final_path);

        assert_ne!(first, second);
        assert_eq!(first.parent(), final_path.parent());
        assert_eq!(second.parent(), final_path.parent());
    }

    #[test]
    #[serial]
    fn watch_state_loads_legacy_map_format() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let legacy = r#"{"Codex":123,"Gemini":456}"#;
        std::fs::write(data_dir.join("watch_state.json"), legacy).unwrap();

        let loaded = load_watch_state(&data_dir);
        assert_eq!(loaded.get(&ConnectorKind::Codex), Some(&123));
        assert_eq!(loaded.get(&ConnectorKind::Gemini), Some(&456));
    }

    #[test]
    #[serial]
    fn watch_state_saves_compact_keys() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let mut state = HashMap::new();
        state.insert(ConnectorKind::Codex, 123);

        save_watch_state(&data_dir, &state).unwrap();

        let raw = std::fs::read_to_string(data_dir.join("watch_state.json")).unwrap();
        assert!(raw.contains("\"m\""));
        assert!(raw.contains("\"cx\""));
        assert!(!raw.contains("Codex"));
    }

    #[test]
    #[serial]
    fn watch_state_updates_after_reindex_paths() {
        let tmp = TempDir::new().unwrap();
        // Use unique subdirectory to avoid conflicts with other tests
        let xdg = tmp.path().join("xdg_watch_state");
        std::fs::create_dir_all(&xdg).unwrap();
        let prev = dotenvy::var("XDG_DATA_HOME").ok();
        unsafe { std::env::set_var("XDG_DATA_HOME", &xdg) };

        // Use xdg directly (not dirs::data_dir() which doesn't respect XDG_DATA_HOME on macOS)
        let data_dir = xdg.join("amp");
        std::fs::create_dir_all(&data_dir).unwrap();

        // Prepare amp fixture under data dir so detection + scan succeed.
        let amp_dir = data_dir.join("amp");
        std::fs::create_dir_all(&amp_dir).unwrap();
        let amp_file = amp_dir.join("thread-002.json");
        std::fs::write(
            &amp_file,
            r#"{
  "id": "thread-002",
  "title": "Amp test",
  "messages": [
    {"role":"user","text":"hi","createdAt":1700000000100},
    {"role":"assistant","text":"hello","createdAt":1700000000200}
  ]
}"#,
        )
        .unwrap();

        let opts = super::IndexOptions {
            full: false,
            watch: false,
            force_rebuild: false,
            db_path: data_dir.join("agent_search.db"),
            data_dir: data_dir.clone(),
            semantic: false,
            build_hnsw: false,
            embedder: "fastembed".to_string(),
            progress: None,
            watch_once_paths: None,
            watch_interval_secs: 30,
        };

        // Manually set up dependencies for reindex_paths
        let storage = FrankenStorage::open(&opts.db_path).unwrap();
        let t_index = TantivyIndex::open_or_create(&index_dir(&opts.data_dir).unwrap()).unwrap();

        let state = std::sync::Mutex::new(std::collections::HashMap::new());
        let storage = std::sync::Mutex::new(storage);
        let t_index = std::sync::Mutex::new(t_index);

        // Need roots for reindex_paths
        let roots = vec![(ConnectorKind::Amp, ScanRoot::local(amp_dir))];

        reindex_paths(
            &opts,
            vec![amp_file.clone()],
            &roots,
            &state,
            &storage,
            &t_index,
            false,
        )
        .unwrap();

        let loaded = load_watch_state(&data_dir);
        assert!(loaded.contains_key(&ConnectorKind::Amp));
        let ts = loaded.get(&ConnectorKind::Amp).copied().unwrap();
        assert!(ts > 0);

        if let Some(prev) = prev {
            unsafe { std::env::set_var("XDG_DATA_HOME", prev) };
        } else {
            unsafe { std::env::remove_var("XDG_DATA_HOME") };
        }
    }

    #[test]
    #[serial]
    fn reindex_paths_uses_oldest_trigger_window_when_state_is_newer() {
        let tmp = TempDir::new().unwrap();
        let xdg = tmp.path().join("xdg_oldest_window");
        std::fs::create_dir_all(&xdg).unwrap();
        let prev = dotenvy::var("XDG_DATA_HOME").ok();
        unsafe { std::env::set_var("XDG_DATA_HOME", &xdg) };

        // Use xdg directly (not dirs::data_dir() which doesn't respect XDG_DATA_HOME on macOS)
        let data_dir = xdg.join("amp");
        std::fs::create_dir_all(&data_dir).unwrap();
        let amp_dir = data_dir.join("amp");
        std::fs::create_dir_all(&amp_dir).unwrap();
        let amp_file = amp_dir.join("thread-window.json");
        let now_u128 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let now = i64::try_from(now_u128)
            .unwrap_or(i64::MAX)
            .saturating_add(10_000);
        std::fs::write(
            &amp_file,
            format!(r#"{{"id":"tw","messages":[{{"role":"user","text":"p","createdAt":{now}}}]}}"#),
        )
        .unwrap();

        let opts = super::IndexOptions {
            full: false,
            watch: false,
            force_rebuild: false,
            watch_once_paths: None,
            db_path: data_dir.join("db.sqlite"),
            data_dir: data_dir.clone(),
            semantic: false,
            build_hnsw: false,
            embedder: "fastembed".to_string(),
            progress: None,
            watch_interval_secs: 30,
        };

        let storage = FrankenStorage::open(&opts.db_path).unwrap();
        let t_index = TantivyIndex::open_or_create(&index_dir(&opts.data_dir).unwrap()).unwrap();
        let mut initial = HashMap::new();
        initial.insert(ConnectorKind::Amp, i64::MAX / 4);
        let state = Mutex::new(initial);
        let storage = Mutex::new(storage);
        let t_index = Mutex::new(t_index);
        let roots = vec![(ConnectorKind::Amp, ScanRoot::local(amp_dir))];

        let indexed = reindex_paths(
            &opts,
            vec![amp_file],
            &roots,
            &state,
            &storage,
            &t_index,
            false,
        )
        .unwrap();
        assert!(
            indexed > 0,
            "expected indexing to use trigger min_ts instead of stale future watch-state"
        );

        if let Some(prev) = prev {
            unsafe { std::env::set_var("XDG_DATA_HOME", prev) };
        } else {
            unsafe { std::env::remove_var("XDG_DATA_HOME") };
        }
    }

    #[test]
    #[serial]
    fn reindex_paths_does_not_advance_watch_state_when_scan_yields_no_conversations() {
        let tmp = TempDir::new().unwrap();
        let xdg = tmp.path().join("xdg_zero_scan");
        std::fs::create_dir_all(&xdg).unwrap();
        let prev = dotenvy::var("XDG_DATA_HOME").ok();
        unsafe { std::env::set_var("XDG_DATA_HOME", &xdg) };

        // Use xdg directly (not dirs::data_dir() which doesn't respect XDG_DATA_HOME on macOS)
        let data_dir = xdg.join("amp");
        std::fs::create_dir_all(&data_dir).unwrap();
        let amp_dir = data_dir.join("amp");
        std::fs::create_dir_all(&amp_dir).unwrap();
        let amp_file = amp_dir.join("thread-zero.json");
        // Intentionally malformed payload so scan yields zero conversations.
        std::fs::write(&amp_file, "not valid json").unwrap();

        let opts = super::IndexOptions {
            full: false,
            watch: false,
            force_rebuild: false,
            watch_once_paths: None,
            db_path: data_dir.join("db.sqlite"),
            data_dir: data_dir.clone(),
            semantic: false,
            build_hnsw: false,
            embedder: "fastembed".to_string(),
            progress: None,
            watch_interval_secs: 30,
        };

        let storage = FrankenStorage::open(&opts.db_path).unwrap();
        let t_index = TantivyIndex::open_or_create(&index_dir(&opts.data_dir).unwrap()).unwrap();
        let mut initial = HashMap::new();
        initial.insert(ConnectorKind::Amp, 10_000);
        let state = Mutex::new(initial);
        let storage = Mutex::new(storage);
        let t_index = Mutex::new(t_index);
        let roots = vec![(ConnectorKind::Amp, ScanRoot::local(amp_dir))];

        let indexed = reindex_paths(
            &opts,
            vec![amp_file],
            &roots,
            &state,
            &storage,
            &t_index,
            false,
        )
        .unwrap();
        assert_eq!(
            indexed, 0,
            "fixture should produce no indexed conversations"
        );
        let guard = state.lock().unwrap();
        assert_eq!(guard.get(&ConnectorKind::Amp), Some(&10_000));

        if let Some(prev) = prev {
            unsafe { std::env::set_var("XDG_DATA_HOME", prev) };
        } else {
            unsafe { std::env::remove_var("XDG_DATA_HOME") };
        }
    }

    #[test]
    #[serial]
    fn reindex_paths_updates_progress() {
        let tmp = TempDir::new().unwrap();
        // Use unique subdirectory to avoid conflicts with other tests
        let xdg = tmp.path().join("xdg_progress");
        std::fs::create_dir_all(&xdg).unwrap();
        let prev = dotenvy::var("XDG_DATA_HOME").ok();
        unsafe { std::env::set_var("XDG_DATA_HOME", &xdg) };

        // Prepare amp fixture using temp directory directly (not dirs::data_dir()
        // which doesn't respect XDG_DATA_HOME on macOS)
        let data_dir = xdg.join("amp");
        std::fs::create_dir_all(&data_dir).unwrap();
        let amp_dir = data_dir.join("amp");
        std::fs::create_dir_all(&amp_dir).unwrap();
        let amp_file = amp_dir.join("thread-progress.json");
        // Use a timestamp well in the future to avoid race with file mtime.
        // The since_ts filter compares message.createdAt > file_mtime - 1, so if
        // there's any delay between capturing 'now' and writing the file, the message
        // could be filtered out. Adding 10s buffer ensures the message is always included.
        let now_u128 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let now = i64::try_from(now_u128)
            .unwrap_or(i64::MAX)
            .saturating_add(10_000);
        std::fs::write(
            &amp_file,
            format!(r#"{{"id":"tp","messages":[{{"role":"user","text":"p","createdAt":{now}}}]}}"#),
        )
        .unwrap();

        let progress = Arc::new(super::IndexingProgress::default());
        let opts = super::IndexOptions {
            full: false,
            watch: false,
            force_rebuild: false,
            watch_once_paths: None,
            db_path: data_dir.join("db.sqlite"),
            data_dir: data_dir.clone(),
            semantic: false,
            build_hnsw: false,
            embedder: "fastembed".to_string(),
            progress: Some(progress.clone()),
            watch_interval_secs: 30,
        };

        let storage = FrankenStorage::open(&opts.db_path).unwrap();
        let t_index = TantivyIndex::open_or_create(&index_dir(&opts.data_dir).unwrap()).unwrap();
        let state = Mutex::new(HashMap::new());
        let storage = Mutex::new(storage);
        let t_index = Mutex::new(t_index);

        reindex_paths(
            &opts,
            vec![amp_file],
            &[(ConnectorKind::Amp, ScanRoot::local(amp_dir))],
            &state,
            &storage,
            &t_index,
            false,
        )
        .unwrap();

        // Progress should reflect the indexed conversation
        assert_eq!(progress.total.load(Ordering::Relaxed), 1);
        assert_eq!(progress.current.load(Ordering::Relaxed), 1);
        // Phase resets to 0 (idle) at the end
        assert_eq!(progress.phase.load(Ordering::Relaxed), 0);

        // Explicitly drop resources to release locks before cleanup
        drop(t_index);
        storage.into_inner().unwrap().close().unwrap();
        drop(state);

        if let Some(prev) = prev {
            unsafe { std::env::set_var("XDG_DATA_HOME", prev) };
        } else {
            unsafe { std::env::remove_var("XDG_DATA_HOME") };
        }
    }

    // P2.2 Tests: Multi-root orchestration and provenance injection

    #[test]
    fn inject_provenance_adds_cass_origin_to_metadata() {
        let mut conv = norm_conv(Some("test"), vec![norm_msg(0, 100)]);
        assert!(conv.metadata.get("cass").is_none());

        let origin = Origin::local();
        inject_provenance(&mut conv, &origin);

        let cass = conv.metadata.get("cass").expect("cass field should exist");
        let origin_obj = cass.get("origin").expect("origin should exist");
        assert_eq!(origin_obj.get("source_id").unwrap().as_str(), Some("local"));
        assert_eq!(origin_obj.get("kind").unwrap().as_str(), Some("local"));
    }

    #[test]
    fn inject_provenance_handles_remote_origin() {
        let mut conv = norm_conv(Some("test"), vec![norm_msg(0, 100)]);

        let origin = Origin::remote_with_host("laptop", "user@laptop.local");
        inject_provenance(&mut conv, &origin);

        let cass = conv.metadata.get("cass").expect("cass field should exist");
        let origin_obj = cass.get("origin").expect("origin should exist");
        assert_eq!(
            origin_obj.get("source_id").unwrap().as_str(),
            Some("laptop")
        );
        assert_eq!(origin_obj.get("kind").unwrap().as_str(), Some("ssh"));
        assert_eq!(
            origin_obj.get("host").unwrap().as_str(),
            Some("user@laptop.local")
        );
    }

    #[test]
    fn extract_provenance_returns_local_for_empty_metadata() {
        let conv = persist::map_to_internal(&NormalizedConversation {
            agent_slug: "test".into(),
            external_id: None,
            title: None,
            workspace: None,
            source_path: PathBuf::from("/test"),
            started_at: None,
            ended_at: None,
            metadata: serde_json::json!({}),
            messages: vec![],
        });
        assert_eq!(conv.source_id, "local");
        assert!(conv.origin_host.is_none());
    }

    #[test]
    fn extract_provenance_extracts_remote_origin() {
        let metadata = serde_json::json!({
            "cass": {
                "origin": {
                    "source_id": "laptop",
                    "kind": "ssh",
                    "host": "user@laptop.local"
                }
            }
        });
        let conv = persist::map_to_internal(&NormalizedConversation {
            agent_slug: "test".into(),
            external_id: None,
            title: None,
            workspace: None,
            source_path: PathBuf::from("/test"),
            started_at: None,
            ended_at: None,
            metadata,
            messages: vec![],
        });
        assert_eq!(conv.source_id, "laptop");
        assert_eq!(conv.origin_host, Some("user@laptop.local".to_string()));
    }

    #[test]
    #[serial]
    fn build_scan_roots_creates_local_root() {
        let _guard = ignore_sources_config();
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let db_path = data_dir.join("db.sqlite");
        let storage = FrankenStorage::open(&db_path).unwrap();

        let roots = build_scan_roots(&storage, &data_dir);

        // Should have at least the local root
        assert!(!roots.is_empty());
        assert_eq!(roots[0].origin.source_id, "local");
        assert!(!roots[0].origin.is_remote());
    }

    #[test]
    #[serial]
    fn build_scan_roots_includes_remote_mirror_if_exists() {
        let _guard = ignore_sources_config();
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        // Create a remote source in the database
        let db_path = data_dir.join("db.sqlite");
        let storage = FrankenStorage::open(&db_path).unwrap();

        // Register a remote source
        storage
            .upsert_source(&crate::sources::provenance::Source {
                id: "laptop".to_string(),
                kind: SourceKind::Ssh,
                host_label: Some("user@laptop.local".to_string()),
                machine_id: None,
                platform: Some("linux".to_string()),
                config_json: None,
                created_at: None,
                updated_at: None,
            })
            .unwrap();

        // Create the mirror directory
        let mirror_dir = data_dir.join("remotes").join("laptop").join("mirror");
        std::fs::create_dir_all(&mirror_dir).unwrap();

        let roots = build_scan_roots(&storage, &data_dir);

        // Should have local root + remote root
        assert_eq!(roots.len(), 2);

        // Find the remote root
        let remote_root = roots.iter().find(|r| r.origin.source_id == "laptop");
        assert!(remote_root.is_some());
        let remote_root = remote_root.unwrap();
        assert!(remote_root.origin.is_remote());
        assert_eq!(
            remote_root.origin.host,
            Some("user@laptop.local".to_string())
        );
        assert_eq!(remote_root.platform, Some(Platform::Linux));
    }

    #[test]
    #[serial]
    fn build_scan_roots_skips_nonexistent_mirror() {
        let _guard = ignore_sources_config();
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        // Register a remote source but don't create mirror directory
        let db_path = data_dir.join("db.sqlite");
        let storage = FrankenStorage::open(&db_path).unwrap();

        // Register a remote source
        storage
            .upsert_source(&crate::sources::provenance::Source {
                id: "nonexistent".to_string(),
                kind: SourceKind::Ssh,
                host_label: Some("user@host".to_string()),
                machine_id: None,
                platform: None,
                config_json: None,
                created_at: None,
                updated_at: None,
            })
            .unwrap();

        // Create the mirror directory but with a different name
        let mirror_dir = data_dir.join("remotes").join("laptop").join("mirror");
        std::fs::create_dir_all(&mirror_dir).unwrap();

        let roots = build_scan_roots(&storage, &data_dir);

        // Should only have local root (remote skipped because mirror doesn't exist)
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].origin.source_id, "local");
    }

    #[test]
    fn apply_workspace_rewrite_no_rewrites() {
        let mut conv = norm_conv(None, vec![norm_msg(0, 1000)]);
        conv.workspace = Some(PathBuf::from("/home/user/projects/app"));

        let root = crate::connectors::ScanRoot::local(PathBuf::from("/"));
        apply_workspace_rewrite(&mut conv, &root);

        // Workspace unchanged when no rewrites
        assert_eq!(
            conv.workspace,
            Some(PathBuf::from("/home/user/projects/app"))
        );
        // No workspace_original in metadata
        assert!(
            conv.metadata
                .get("cass")
                .and_then(|c| c.get("workspace_original"))
                .is_none()
        );
    }

    #[test]
    fn apply_workspace_rewrite_no_workspace() {
        let mut conv = norm_conv(None, vec![norm_msg(0, 1000)]);
        conv.workspace = None;

        let mappings = vec![crate::sources::config::PathMapping::new(
            "/home/user",
            "/Users/me",
        )];

        let mut root = crate::connectors::ScanRoot::local(PathBuf::from("/"));
        root.workspace_rewrites = mappings;
        apply_workspace_rewrite(&mut conv, &root);

        // Still None
        assert!(conv.workspace.is_none());
    }

    #[test]
    fn apply_workspace_rewrite_applies_mapping() {
        let mut conv = norm_conv(None, vec![norm_msg(0, 1000)]);
        conv.workspace = Some(PathBuf::from("/home/user/projects/app"));

        let mappings = vec![crate::sources::config::PathMapping::new(
            "/home/user",
            "/Users/me",
        )];

        let mut root = crate::connectors::ScanRoot::local(PathBuf::from("/"));
        root.workspace_rewrites = mappings;
        apply_workspace_rewrite(&mut conv, &root);

        // Workspace rewritten
        assert_eq!(
            conv.workspace,
            Some(PathBuf::from("/Users/me/projects/app"))
        );

        // Original stored in metadata
        let workspace_original = conv
            .metadata
            .get("cass")
            .and_then(|c| c.get("workspace_original"))
            .and_then(|v| v.as_str());
        assert_eq!(workspace_original, Some("/home/user/projects/app"));
    }

    #[test]
    fn apply_workspace_rewrite_longest_prefix_match() {
        let mut conv = norm_conv(None, vec![norm_msg(0, 1000)]);
        conv.workspace = Some(PathBuf::from("/home/user/projects/special/app"));

        let mappings = vec![crate::sources::config::PathMapping::new(
            "/home/user",
            "/Users/me",
        )];

        let mut root = crate::connectors::ScanRoot::local(PathBuf::from("/"));
        root.workspace_rewrites = mappings;
        apply_workspace_rewrite(&mut conv, &root);

        // Should use longest prefix match
        assert_eq!(
            conv.workspace,
            Some(PathBuf::from("/Users/me/projects/special/app"))
        );
    }

    #[test]
    fn apply_workspace_rewrite_no_match() {
        let mut conv = norm_conv(None, vec![norm_msg(0, 1000)]);
        conv.workspace = Some(PathBuf::from("/opt/other/path"));

        let mappings = vec![crate::sources::config::PathMapping::new(
            "/home/user",
            "/Users/me",
        )];

        let mut root = crate::connectors::ScanRoot::local(PathBuf::from("/"));
        root.workspace_rewrites = mappings;
        apply_workspace_rewrite(&mut conv, &root);

        // Workspace unchanged - no matching prefix
        assert_eq!(conv.workspace, Some(PathBuf::from("/opt/other/path")));
        // No workspace_original since nothing was rewritten
        assert!(
            conv.metadata
                .get("cass")
                .and_then(|c| c.get("workspace_original"))
                .is_none()
        );
    }

    #[test]
    fn apply_workspace_rewrite_with_agent_filter() {
        // Test with agent filter
        let mut conv = norm_conv(None, vec![norm_msg(0, 1000)]);
        conv.agent_slug = "claude-code".to_string();
        conv.workspace = Some(PathBuf::from("/home/user/projects/app"));

        let mappings = vec![crate::sources::config::PathMapping::with_agents(
            "/home/user/projects",
            "/Volumes/Work",
            vec!["cursor".to_string()], // Only for cursor, not claude-code
        )];

        let mut root = crate::connectors::ScanRoot::local(PathBuf::from("/"));
        root.workspace_rewrites = mappings;
        apply_workspace_rewrite(&mut conv, &root);

        // Should NOT use cursor-specific mapping, falls back to general
        assert_eq!(
            conv.workspace,
            Some(PathBuf::from("/home/user/projects/app"))
        );
    }

    #[test]
    fn apply_workspace_rewrite_preserves_existing_metadata() {
        let mut conv = norm_conv(None, vec![norm_msg(0, 1000)]);
        conv.workspace = Some(PathBuf::from("/home/user/app"));
        conv.metadata = serde_json::json!({
            "cass": {
                "origin": {
                    "source_id": "laptop",
                    "kind": "ssh",
                    "host": "user@laptop.local"
                }
            }
        });

        let mappings = vec![crate::sources::config::PathMapping::new(
            "/home/user",
            "/Users/me",
        )];

        let mut root = crate::connectors::ScanRoot::local(PathBuf::from("/"));
        root.workspace_rewrites = mappings;
        apply_workspace_rewrite(&mut conv, &root);

        // Origin preserved
        assert_eq!(
            conv.metadata["cass"]["origin"]["source_id"].as_str(),
            Some("laptop")
        );
        // workspace_original added
        assert_eq!(
            conv.metadata["cass"]["workspace_original"].as_str(),
            Some("/home/user/app")
        );
    }

    // =========================================================================
    // Stale Detection Tests
    // =========================================================================

    #[test]
    fn stale_action_from_env_str_parses_correctly() {
        assert_eq!(StaleAction::from_env_str("warn"), StaleAction::Warn);
        assert_eq!(StaleAction::from_env_str("WARN"), StaleAction::Warn);
        assert_eq!(StaleAction::from_env_str("rebuild"), StaleAction::Rebuild);
        assert_eq!(StaleAction::from_env_str("auto"), StaleAction::Rebuild);
        assert_eq!(StaleAction::from_env_str("fix"), StaleAction::Rebuild);
        assert_eq!(StaleAction::from_env_str("none"), StaleAction::None);
        assert_eq!(StaleAction::from_env_str("off"), StaleAction::None);
        assert_eq!(StaleAction::from_env_str("0"), StaleAction::None);
        assert_eq!(StaleAction::from_env_str("false"), StaleAction::None);
        assert_eq!(StaleAction::from_env_str("unknown"), StaleAction::Warn);
    }

    #[test]
    fn stale_config_default_values() {
        let cfg = StaleConfig::default();
        assert_eq!(cfg.threshold_hours, 24);
        assert_eq!(cfg.action, StaleAction::Warn);
        assert_eq!(cfg.check_interval_mins, 60);
        assert_eq!(cfg.min_zero_scans, 10);
        assert!(cfg.is_enabled());
    }

    #[test]
    fn stale_config_none_action_disables_detection() {
        let cfg = StaleConfig {
            action: StaleAction::None,
            ..Default::default()
        };
        assert!(!cfg.is_enabled());
    }

    #[test]
    fn stale_detector_records_successful_ingest() {
        let detector = StaleDetector::new(StaleConfig::default());
        assert_eq!(detector.stats().total_ingests, 0);
        assert_eq!(detector.stats().consecutive_zero_scans, 0);

        detector.record_scan(5);
        assert_eq!(detector.stats().total_ingests, 1);
        assert_eq!(detector.stats().consecutive_zero_scans, 0);
        assert!(detector.stats().seconds_since_last_ingest.is_some());
    }

    #[test]
    fn stale_detector_tracks_zero_scans() {
        let detector = StaleDetector::new(StaleConfig::default());

        detector.record_scan(0);
        assert_eq!(detector.stats().consecutive_zero_scans, 1);

        detector.record_scan(0);
        assert_eq!(detector.stats().consecutive_zero_scans, 2);

        // Successful scan resets counter
        detector.record_scan(1);
        assert_eq!(detector.stats().consecutive_zero_scans, 0);
    }

    #[test]
    fn stale_detector_reset_clears_state() {
        let detector = StaleDetector::new(StaleConfig::default());

        detector.record_scan(0);
        detector.record_scan(0);
        assert_eq!(detector.stats().consecutive_zero_scans, 2);

        detector.reset();
        assert_eq!(detector.stats().consecutive_zero_scans, 0);
        assert!(detector.stats().seconds_since_last_ingest.is_some());
    }

    #[test]
    fn finalize_watch_reindex_result_records_error_and_resets_phase() {
        let detector = StaleDetector::new(StaleConfig::default());
        let progress = Arc::new(IndexingProgress::default());
        progress.phase.store(2, Ordering::Relaxed);

        let indexed = finalize_watch_reindex_result(
            Err(anyhow::anyhow!("boom")),
            &detector,
            Some(&progress),
            "watch incremental reindex",
        );

        assert_eq!(
            indexed, 0,
            "failed watch reindex should report zero indexed"
        );
        assert_eq!(
            detector.stats().consecutive_zero_scans,
            1,
            "failed watch reindex should still count as a zero-result scan for stale detection"
        );
        assert_eq!(
            progress.phase.load(Ordering::Relaxed),
            0,
            "failed watch reindex should reset progress phase back to idle"
        );
        assert_eq!(
            progress
                .last_error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_deref(),
            Some("watch incremental reindex: boom"),
            "failed watch reindex should surface the real error"
        );
    }

    #[test]
    fn run_index_progress_reset_guard_resets_idle_state_without_clobbering_error() {
        let progress = Arc::new(IndexingProgress::default());
        progress.phase.store(2, Ordering::Relaxed);
        progress.is_rebuilding.store(true, Ordering::Relaxed);
        *progress
            .last_error
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some("boom".to_string());

        {
            let _guard = RunIndexProgressReset::new(Some(progress.clone()));
        }

        assert_eq!(progress.phase.load(Ordering::Relaxed), 0);
        assert!(
            !progress.is_rebuilding.load(Ordering::Relaxed),
            "drop guard should clear stale rebuild state after failures"
        );
        assert_eq!(
            progress
                .last_error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_deref(),
            Some("boom"),
            "idle-state cleanup should not erase the real error"
        );
    }

    #[test]
    fn reconcile_pending_lexical_commit_promotes_committed_offset_when_meta_changes() {
        let tmp = TempDir::new().unwrap();
        let index_path = tmp.path().join("index");
        fs::create_dir_all(&index_path).unwrap();
        fs::write(index_path.join("meta.json"), b"before").unwrap();

        let db_state = LexicalRebuildDbState {
            db_path: "/tmp/agent_search.db".to_string(),
            total_conversations: 400,
            storage_fingerprint: "seed:400".to_string(),
        };
        let mut state = LexicalRebuildState::new(db_state, LEXICAL_REBUILD_PAGE_SIZE);
        state.record_pending_commit(200, 200, 600, index_meta_fingerprint(&index_path).unwrap());
        persist_lexical_rebuild_state(&index_path, &state).unwrap();

        fs::write(index_path.join("meta.json"), b"after").unwrap();

        let reconciled = reconcile_pending_lexical_commit(&index_path, state).unwrap();
        assert_eq!(reconciled.committed_offset, 200);
        assert_eq!(reconciled.processed_conversations, 200);
        assert_eq!(reconciled.indexed_docs, 600);
        assert!(reconciled.pending.is_none());
    }

    #[test]
    fn reconcile_pending_lexical_commit_rolls_back_uncommitted_batch_when_meta_unchanged() {
        let tmp = TempDir::new().unwrap();
        let index_path = tmp.path().join("index");
        fs::create_dir_all(&index_path).unwrap();
        fs::write(index_path.join("meta.json"), b"stable").unwrap();

        let db_state = LexicalRebuildDbState {
            db_path: "/tmp/agent_search.db".to_string(),
            total_conversations: 400,
            storage_fingerprint: "seed:400".to_string(),
        };
        let mut state = LexicalRebuildState::new(db_state.clone(), LEXICAL_REBUILD_PAGE_SIZE);
        state.committed_offset = 100;
        state.processed_conversations = 100;
        state.indexed_docs = 250;
        state.record_pending_commit(200, 200, 600, index_meta_fingerprint(&index_path).unwrap());
        persist_lexical_rebuild_state(&index_path, &state).unwrap();

        let reconciled = reconcile_pending_lexical_commit(&index_path, state).unwrap();
        assert_eq!(reconciled.committed_offset, 100);
        assert_eq!(reconciled.processed_conversations, 100);
        assert_eq!(reconciled.indexed_docs, 250);
        assert!(reconciled.pending.is_none());
        assert!(has_pending_lexical_rebuild(&index_path, &db_state).unwrap());
    }

    #[test]
    fn clear_lexical_rebuild_state_removes_stale_snapshot() {
        let tmp = TempDir::new().unwrap();
        let index_path = tmp.path().join("index");
        fs::create_dir_all(&index_path).unwrap();

        let state = LexicalRebuildState::new(
            LexicalRebuildDbState {
                db_path: "/tmp/agent_search.db".to_string(),
                total_conversations: 12,
                storage_fingerprint: "seed:12".to_string(),
            },
            LEXICAL_REBUILD_PAGE_SIZE,
        );
        persist_lexical_rebuild_state(&index_path, &state).unwrap();
        assert!(
            load_lexical_rebuild_snapshot(&index_path, Path::new("/tmp/agent_search.db"))
                .unwrap()
                .is_some()
        );

        clear_lexical_rebuild_state(&index_path).unwrap();
        assert!(
            load_lexical_rebuild_snapshot(&index_path, Path::new("/tmp/agent_search.db"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn should_commit_lexical_rebuild_when_message_count_threshold_is_hit() {
        assert!(should_commit_lexical_rebuild(
            10,
            5_000,
            1_024,
            1_000,
            5_000,
            16 * 1024 * 1024
        ));
        assert!(!should_commit_lexical_rebuild(
            10,
            4_999,
            1_024,
            1_000,
            5_000,
            16 * 1024 * 1024
        ));
    }

    #[test]
    fn should_commit_lexical_rebuild_when_message_byte_threshold_is_hit() {
        assert!(should_commit_lexical_rebuild(
            10,
            100,
            16 * 1024 * 1024,
            1_000,
            5_000,
            16 * 1024 * 1024
        ));
        assert!(!should_commit_lexical_rebuild(
            10,
            100,
            (16 * 1024 * 1024) - 1,
            1_000,
            5_000,
            16 * 1024 * 1024
        ));
    }

    #[test]
    fn finalize_watch_reindex_result_clears_stale_error_on_success() {
        let detector = StaleDetector::new(StaleConfig::default());
        let progress = Arc::new(IndexingProgress::default());
        *progress
            .last_error
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some("old".to_string());

        let indexed = finalize_watch_reindex_result(
            Ok(3),
            &detector,
            Some(&progress),
            "watch incremental reindex",
        );

        assert_eq!(indexed, 3);
        assert_eq!(detector.stats().total_ingests, 1);
        assert_eq!(
            progress
                .last_error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_deref(),
            None,
            "successful watch reindex should clear stale error diagnostics"
        );
    }

    #[test]
    fn stale_detector_check_respects_disabled() {
        let detector = StaleDetector::new(StaleConfig {
            action: StaleAction::None,
            ..Default::default()
        });

        // Even with many zero scans, should return None when disabled
        for _ in 0..20 {
            detector.record_scan(0);
        }
        assert!(detector.check_stale().is_none());
    }

    #[test]
    fn stale_detector_requires_min_zero_scans() {
        let detector = StaleDetector::new(StaleConfig {
            min_zero_scans: 5,
            check_interval_mins: 0, // Disable interval check for test
            threshold_hours: 0,     // Immediate threshold for test
            ..Default::default()
        });

        // Not enough zero scans yet
        for _ in 0..4 {
            detector.record_scan(0);
        }
        assert!(detector.check_stale().is_none());

        // One more to meet threshold
        detector.record_scan(0);
        // Now should trigger (if interval allows)
        // Note: check_stale has its own interval check, so this test is limited
    }

    #[test]
    fn stale_stats_serializes_correctly() {
        let stats = StaleStats {
            consecutive_zero_scans: 5,
            total_ingests: 10,
            seconds_since_last_ingest: Some(3600),
            warning_emitted: true,
            config_action: "Warn".to_string(),
            config_threshold_hours: 24,
        };

        let json = serde_json::to_string(&stats).unwrap();
        assert!(json.contains("consecutive_zero_scans"));
        assert!(json.contains("total_ingests"));
    }
}
