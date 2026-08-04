//! Shared search asset state evaluation for status, health, and fail-open search planning.
//!
//! This module centralizes coarse-grained asset truth so callers stop inferring
//! lexical freshness, active maintenance, and semantic readiness from ad hoc
//! file checks spread across the CLI.
//!
//! The maintenance coordination layer (`evaluate_maintenance_coordination`,
//! `decide_maintenance_action`, `poll_maintenance_until_idle`) provides
//! single-flight semantics: foreground cass actors share one coherent truth
//! for repair/acquisition work and never duplicate basic maintenance jobs.

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use fs2::FileExt;

use crate::indexer::{
    LEXICAL_REBUILD_PAGE_SIZE_PUBLIC, LexicalRebuildCheckpoint,
    lexical_rebuild_page_size_is_compatible, lexical_storage_fingerprint_for_db,
    load_lexical_rebuild_checkpoint,
};
use crate::search::ann_index::hnsw_index_path;
use crate::search::embedder::Embedder;
use crate::search::fastembed_embedder::FastEmbedder;
use crate::search::hash_embedder::HashEmbedder;
use crate::search::model_manager::{
    SemanticAvailability, probe_hash_semantic_availability, probe_semantic_availability,
};
use crate::search::policy::{
    CHUNKING_STRATEGY_VERSION, CliSemanticOverrides, SEMANTIC_SCHEMA_VERSION, SemanticPolicy,
};
use crate::search::semantic_manifest::{
    ArtifactRecord, BuildCheckpoint, SemanticManifest, SemanticShardManifest, SemanticShardRecord,
    TierKind, semantic_shard_artifact_path_is_safe,
};
use crate::search::tantivy::SCHEMA_HASH;
use crate::search::vector_index::{VECTOR_INDEX_DIR, vector_index_path};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub(crate) enum SearchMaintenanceMode {
    Index,
    WatchStartup,
    Watch,
    WatchOnce,
}

impl SearchMaintenanceMode {
    pub(crate) fn as_lock_value(self) -> &'static str {
        match self {
            Self::Index => "index",
            Self::WatchStartup => "watch_startup",
            Self::Watch => "watch",
            Self::WatchOnce => "watch_once",
        }
    }

    pub(crate) fn parse_lock_value(raw: &str) -> Option<Self> {
        match raw.trim() {
            "index" => Some(Self::Index),
            "watch_startup" => Some(Self::WatchStartup),
            "watch" => Some(Self::Watch),
            "watch_once" => Some(Self::WatchOnce),
            _ => None,
        }
    }

    pub(crate) fn watch_active(self) -> bool {
        matches!(self, Self::WatchStartup | Self::Watch)
    }

    pub(crate) fn rebuild_active(self) -> bool {
        !matches!(self, Self::Watch)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub(crate) enum SearchMaintenanceJobKind {
    LexicalRefresh,
    SemanticAcquire,
}

impl SearchMaintenanceJobKind {
    pub(crate) fn as_lock_value(self) -> &'static str {
        match self {
            Self::LexicalRefresh => "lexical_refresh",
            Self::SemanticAcquire => "semantic_acquire",
        }
    }

    pub(crate) fn parse_lock_value(raw: &str) -> Option<Self> {
        match raw.trim() {
            "lexical_refresh" => Some(Self::LexicalRefresh),
            "semantic_acquire" => Some(Self::SemanticAcquire),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub(crate) struct SearchMaintenanceSnapshot {
    pub active: bool,
    pub pid: Option<u32>,
    pub started_at_ms: Option<i64>,
    pub db_path: Option<PathBuf>,
    pub mode: Option<SearchMaintenanceMode>,
    pub job_id: Option<String>,
    pub job_kind: Option<SearchMaintenanceJobKind>,
    pub phase: Option<String>,
    pub updated_at_ms: Option<i64>,
    pub last_progress_at_ms: Option<i64>,
    pub orphaned: bool,
}

pub(crate) fn index_run_lock_path(data_dir: &Path) -> PathBuf {
    data_dir.join("index-run.lock")
}

pub(crate) fn index_run_lock_metadata_sidecar_path(lock_path: &Path) -> PathBuf {
    lock_path.with_file_name("index-run.lock.meta")
}

pub(crate) fn write_index_run_lock_metadata_sidecar(
    lock_path: &Path,
    contents: &str,
) -> Result<()> {
    let sidecar_path = index_run_lock_metadata_sidecar_path(lock_path);
    let mut sidecar = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&sidecar_path)
        .with_context(|| {
            format!(
                "opening index-run lock metadata sidecar {}",
                sidecar_path.display()
            )
        })?;
    sidecar.write_all(contents.as_bytes()).with_context(|| {
        format!(
            "writing index-run lock metadata sidecar {}",
            sidecar_path.display()
        )
    })?;
    sidecar.flush().with_context(|| {
        format!(
            "flushing index-run lock metadata sidecar {}",
            sidecar_path.display()
        )
    })?;
    sidecar.sync_all().with_context(|| {
        format!(
            "syncing index-run lock metadata sidecar {}",
            sidecar_path.display()
        )
    })
}

pub(crate) fn clear_index_run_lock_metadata_sidecar(lock_path: &Path) -> Result<()> {
    let sidecar_path = index_run_lock_metadata_sidecar_path(lock_path);
    let sidecar = match OpenOptions::new()
        .truncate(true)
        .write(true)
        .open(&sidecar_path)
    {
        Ok(sidecar) => sidecar,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(err).with_context(|| {
                format!(
                    "opening index-run lock metadata sidecar {}",
                    sidecar_path.display()
                )
            });
        }
    };
    sidecar.sync_all().with_context(|| {
        format!(
            "syncing cleared index-run lock metadata sidecar {}",
            sidecar_path.display()
        )
    })
}

fn read_capped_metadata_from_path(path: &Path, max_len: u64) -> std::io::Result<String> {
    let file = OpenOptions::new().read(true).open(path)?;
    let mut raw = String::new();
    (&file).take(max_len).read_to_string(&mut raw)?;
    Ok(raw)
}

#[cfg(windows)]
pub(crate) fn windows_lock_conflict(err: &std::io::Error) -> bool {
    matches!(err.raw_os_error(), Some(32 | 33))
}

#[cfg(not(windows))]
pub(crate) fn windows_lock_conflict(_err: &std::io::Error) -> bool {
    false
}

pub(crate) fn read_search_maintenance_snapshot(data_dir: &Path) -> SearchMaintenanceSnapshot {
    // Real index-run.lock files written by `acquire_index_run_lock`
    // have a fixed key=value shape under ~1 KiB. Cap the read at 64 KiB
    // so a corrupted or maliciously-large lock file cannot force us to
    // allocate arbitrary memory just to inspect its metadata.
    const MAX_LOCK_FILE_READ: u64 = 64 * 1024;

    let lock_path = index_run_lock_path(data_dir);
    let sidecar_path = index_run_lock_metadata_sidecar_path(&lock_path);
    let file = match OpenOptions::new().read(true).write(true).open(&lock_path) {
        Ok(file) => file,
        Err(err) if windows_lock_conflict(&err) => {
            let raw = read_capped_metadata_from_path(&sidecar_path, MAX_LOCK_FILE_READ)
                .unwrap_or_default();
            return parse_search_maintenance_snapshot(raw, true);
        }
        Err(_) => return SearchMaintenanceSnapshot::default(),
    };

    let mut raw = String::new();
    let mut read_blocked_by_lock = false;
    if let Err(err) = (&file).take(MAX_LOCK_FILE_READ).read_to_string(&mut raw)
        && windows_lock_conflict(&err)
    {
        read_blocked_by_lock = true;
    }
    if raw.trim().is_empty() {
        raw = read_capped_metadata_from_path(&sidecar_path, MAX_LOCK_FILE_READ).unwrap_or_default();
    }

    let mut snapshot = parse_search_maintenance_snapshot(raw, read_blocked_by_lock);
    if read_blocked_by_lock {
        return snapshot;
    }

    let metadata_present = snapshot.pid.is_some()
        || snapshot.started_at_ms.is_some()
        || snapshot.db_path.is_some()
        || snapshot.mode.is_some()
        || snapshot.job_id.is_some()
        || snapshot.job_kind.is_some()
        || snapshot.phase.is_some()
        || snapshot.updated_at_ms.is_some()
        || snapshot.last_progress_at_ms.is_some();

    #[cfg(windows)]
    let current_process_owns_recorded_lock =
        metadata_present && snapshot.pid == Some(std::process::id());
    #[cfg(not(windows))]
    let current_process_owns_recorded_lock = false;

    snapshot.active = if current_process_owns_recorded_lock {
        true
    } else {
        match file.try_lock_exclusive() {
            Ok(()) => {
                // We acquired the exclusive lock with no waiting, which is
                // proof that no process holds it. POSIX flock (via fs2) is
                // released automatically when the owning file description
                // is closed — either explicitly on graceful drop, or by the
                // kernel on process exit / crash. Therefore, if the file
                // contains metadata but no holder is present, the previous
                // owner is gone.
                //
                // Historically this produced a permanent `orphaned: true`
                // state that callers (notably the TUI) interpreted as
                // "rebuild in progress, keep polling" — yielding a tight
                // CPU-bound loop that only cleared when the user manually
                // deleted the lock file (see issue #176).
                //
                // Reap the stale metadata in place while we hold the lock,
                // so that this and every subsequent reader observes a
                // clean state.
                //
                // We deliberately do NOT gate this on a `kill(pid, 0)`
                // liveness probe. Under PID reuse (the recorded pid is
                // reassigned to an unrelated live process), such a probe
                // would refuse to reap and the spin would reappear. Flock
                // acquisition is the stronger and more precise signal.
                if metadata_present {
                    if let Err(err) = file.set_len(0) {
                        tracing::warn!(
                            path = %lock_path.display(),
                            error = %err,
                            "failed to truncate stale index-run lock metadata"
                        );
                    } else {
                        let _ = clear_index_run_lock_metadata_sidecar(&lock_path);
                        let _ = file.sync_all();
                        tracing::info!(
                            path = %lock_path.display(),
                            stale_pid = ?snapshot.pid,
                            "cleared stale index-run lock metadata (previous owner gone)"
                        );
                        let _ = file.unlock();
                        return SearchMaintenanceSnapshot::default();
                    }
                }
                let _ = file.unlock();
                false
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => true,
            Err(err) if windows_lock_conflict(&err) => true,
            Err(_) => false,
        }
    };
    snapshot.orphaned = metadata_present && !snapshot.active;
    snapshot
}

fn parse_search_maintenance_snapshot(
    raw: String,
    active_when_metadata_unreadable: bool,
) -> SearchMaintenanceSnapshot {
    let mut pid = None;
    let mut started_at_ms = None;
    let mut lock_db_path = None::<PathBuf>;
    let mut mode = None;
    let mut job_id = None;
    let mut job_kind = None;
    let mut phase = None;
    let mut updated_at_ms = None;
    let mut last_progress_at_ms = None;
    for line in raw.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "pid" => pid = value.trim().parse::<u32>().ok(),
            "started_at_ms" => started_at_ms = value.trim().parse::<i64>().ok(),
            "db_path" => lock_db_path = Some(PathBuf::from(value.trim())),
            "mode" => mode = SearchMaintenanceMode::parse_lock_value(value),
            "job_id" => job_id = Some(value.trim().to_string()).filter(|value| !value.is_empty()),
            "job_kind" => job_kind = SearchMaintenanceJobKind::parse_lock_value(value),
            "phase" => phase = Some(value.trim().to_string()).filter(|value| !value.is_empty()),
            "updated_at_ms" => updated_at_ms = value.trim().parse::<i64>().ok(),
            "last_progress_at_ms" => last_progress_at_ms = value.trim().parse::<i64>().ok(),
            _ => {}
        }
    }

    let metadata_present = pid.is_some()
        || started_at_ms.is_some()
        || lock_db_path.is_some()
        || mode.is_some()
        || job_id.is_some()
        || job_kind.is_some()
        || phase.is_some()
        || updated_at_ms.is_some()
        || last_progress_at_ms.is_some();

    SearchMaintenanceSnapshot {
        active: active_when_metadata_unreadable,
        pid,
        started_at_ms,
        db_path: lock_db_path,
        mode,
        job_id,
        job_kind,
        phase,
        updated_at_ms,
        last_progress_at_ms,
        orphaned: metadata_present && !active_when_metadata_unreadable,
    }
}

pub(crate) const REBUILD_STALL_DETECT_SECS_DEFAULT: u64 = 120;

pub(crate) fn rebuild_stall_detect_threshold_ms() -> Option<i64> {
    let threshold_secs = dotenvy::var("CASS_REBUILD_STALL_DETECT_SECS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(REBUILD_STALL_DETECT_SECS_DEFAULT);
    if threshold_secs == 0 {
        return None;
    }
    let threshold_ms = threshold_secs.saturating_mul(1000);
    Some(i64::try_from(threshold_ms).unwrap_or(i64::MAX))
}

pub(crate) fn maintenance_stall_age_ms(
    snapshot: &SearchMaintenanceSnapshot,
    now_ms: i64,
) -> Option<i64> {
    if !snapshot.active
        || !snapshot
            .mode
            .is_some_and(SearchMaintenanceMode::rebuild_active)
    {
        return None;
    }
    let last_progress_at_ms = snapshot.last_progress_at_ms?;
    let age_ms = now_ms.saturating_sub(last_progress_at_ms);
    let threshold_ms = rebuild_stall_detect_threshold_ms()?;
    (age_ms >= threshold_ms).then_some(age_ms)
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SemanticPreference {
    DefaultModel,
    HashFallback,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SearchAssetSnapshot {
    pub lexical: LexicalAssetState,
    pub semantic: SemanticAssetState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LexicalAssetState {
    pub status: &'static str,
    pub exists: bool,
    pub fresh: bool,
    pub stale: bool,
    pub rebuilding: bool,
    /// `true` when the rebuild is nominally active but the indexing
    /// thread has not posted forward progress within the configured
    /// stall threshold. Driven by `last_progress_at_ms` on the lock
    /// file, NOT by `updated_at_ms` (which the heartbeat thread
    /// refreshes unconditionally). See issue #258 for the regression
    /// this guards against.
    pub stalled: bool,
    /// Wall-clock age of the most recent forward-progress event, when
    /// the indexer thread has posted one. `None` when no
    /// `last_progress_at_ms` is available (legacy lock file, or the
    /// rebuild is not active).
    pub last_progress_age_ms: Option<i64>,
    pub last_progress_at_ms: Option<i64>,
    pub watch_active: bool,
    pub last_indexed_at_ms: Option<i64>,
    pub age_seconds: Option<u64>,
    pub stale_threshold_seconds: u64,
    pub activity_at_ms: Option<i64>,
    pub pending_sessions: u64,
    pub processed_conversations: Option<u64>,
    pub total_conversations: Option<u64>,
    pub indexed_docs: Option<u64>,
    pub status_reason: Option<String>,
    pub fingerprint: LexicalFingerprintState,
    pub checkpoint: LexicalCheckpointState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LexicalFingerprintState {
    pub current_db_fingerprint: Option<String>,
    pub checkpoint_fingerprint: Option<String>,
    pub matches_current_db_fingerprint: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LexicalCheckpointState {
    pub present: bool,
    pub completed: Option<bool>,
    pub db_matches: Option<bool>,
    pub schema_matches: Option<bool>,
    pub page_size_matches: Option<bool>,
    pub page_size_compatible: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SemanticAssetState {
    pub status: &'static str,
    pub availability: &'static str,
    pub summary: String,
    pub available: bool,
    pub can_search: bool,
    pub fallback_mode: Option<&'static str>,
    pub preferred_backend: &'static str,
    pub embedder_id: Option<String>,
    pub vector_index_path: Option<PathBuf>,
    pub model_dir: Option<PathBuf>,
    pub hnsw_path: Option<PathBuf>,
    pub hnsw_ready: bool,
    pub progressive_ready: bool,
    /// Sub-fix 3 for cass#257: true when a quality-tier vector index is
    /// published, matches the current DB fingerprint, and could serve a
    /// `--mode semantic` search even if the progressive/hybrid stack is
    /// still building (e.g. the fast tier hasn't been backfilled yet).
    ///
    /// Distinct from `progressive_ready`, which only returns true when
    /// BOTH the fast and quality tier index files exist on disk —
    /// useful for the "hybrid stack is good to go" surface but
    /// misleading for operators who only run `--mode semantic`.
    pub quality_tier_published: bool,
    /// Sub-fix 3 for cass#257: true when at least one tier (fast OR
    /// quality) is queryable against the current DB. This collapses
    /// the per-tier readiness into a single flag suitable for the
    /// operator question "can I run `cass search --mode semantic`
    /// right now?". Mirrors `can_search` but is named so it survives
    /// future refactors of the can_search semantics.
    pub semantic_only_search_available: bool,
    pub hint: Option<String>,
    pub fast_tier: SemanticTierAssetState,
    pub quality_tier: SemanticTierAssetState,
    pub backlog: SemanticBacklogProgressState,
    pub checkpoint: SemanticCheckpointProgressState,
}

struct SemanticRuntimeSurface {
    status: &'static str,
    availability: &'static str,
    summary: String,
    can_search: bool,
    fallback_mode: Option<&'static str>,
    hint: Option<String>,
    embedder_id: Option<String>,
    vector_index_path: Option<PathBuf>,
    model_dir: Option<PathBuf>,
    hnsw_path: Option<PathBuf>,
}

struct SemanticRuntimeInputs<'a> {
    data_dir: &'a Path,
    availability: &'a SemanticAvailability,
    preference: SemanticPreference,
    fast_tier: &'a SemanticTierAssetState,
    quality_tier: &'a SemanticTierAssetState,
    backlog: &'a SemanticBacklogProgressState,
    checkpoint: &'a SemanticCheckpointProgressState,
    base_embedder_id: Option<String>,
    base_vector_index_path: Option<PathBuf>,
    base_model_dir: Option<PathBuf>,
    base_hnsw_path: Option<PathBuf>,
}

struct SemanticPreferenceSurface {
    preferred_backend: &'static str,
    model_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SemanticTierAssetState {
    pub present: bool,
    pub ready: bool,
    pub current_db_matches: Option<bool>,
    pub conversation_count: Option<u64>,
    pub doc_count: Option<u64>,
    pub embedder_id: Option<String>,
    pub model_revision: Option<String>,
    pub completed_at_ms: Option<i64>,
    pub size_bytes: Option<u64>,
    pub index_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SemanticBacklogProgressState {
    pub total_conversations: u64,
    pub fast_tier_processed: u64,
    pub fast_tier_remaining: u64,
    pub quality_tier_processed: u64,
    pub quality_tier_remaining: u64,
    pub pending_work: bool,
    pub current_db_matches: Option<bool>,
    pub computed_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SemanticCheckpointProgressState {
    pub active: bool,
    pub tier: Option<&'static str>,
    pub current_db_matches: Option<bool>,
    pub completed: Option<bool>,
    pub conversations_processed: Option<u64>,
    pub total_conversations: Option<u64>,
    pub progress_pct: Option<u8>,
    pub docs_embedded: Option<u64>,
    pub last_offset: Option<i64>,
    pub saved_at_ms: Option<i64>,
}

pub(crate) struct InspectSearchAssetsInput<'a> {
    pub data_dir: &'a Path,
    pub db_path: &'a Path,
    pub stale_threshold: u64,
    pub last_indexed_at_ms: Option<i64>,
    /// Full-precision (millisecond) wall clock used for stall-detection
    /// math against `last_progress_at_ms`. Callers should pass
    /// `FrankenStorage::now_millis()` here. F4 (cass tech debt): the
    /// previous shape only carried `now_secs`, which quantised the
    /// comparison to second resolution while `last_progress_at_ms` is
    /// stored at full ms. Down-stream `now_secs` is now derived from
    /// this value so the two clocks remain consistent.
    pub now_ms: i64,
    pub maintenance: SearchMaintenanceSnapshot,
    pub semantic_preference: SemanticPreference,
    pub db_available: bool,
    pub compute_lexical_fingerprint: bool,
    pub inspect_semantic: bool,
}

const LEXICAL_STORAGE_FINGERPRINT_MTIME_TOLERANCE_MS: i64 = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ParsedLexicalStorageFingerprint {
    db_len: u64,
    db_mtime_ms: i64,
    wal_len: u64,
    wal_mtime_ms: i64,
}

fn parse_lexical_storage_fingerprint(raw: &str) -> Option<ParsedLexicalStorageFingerprint> {
    let mut parts = raw.split(':');
    let fingerprint = ParsedLexicalStorageFingerprint {
        db_len: parts.next()?.parse().ok()?,
        db_mtime_ms: parts.next()?.parse().ok()?,
        wal_len: parts.next()?.parse().ok()?,
        wal_mtime_ms: parts.next()?.parse().ok()?,
    };
    if parts.next().is_some() {
        return None;
    }
    Some(fingerprint)
}

pub(crate) fn lexical_storage_fingerprints_match(current: &str, saved: &str) -> bool {
    match (
        parse_lexical_storage_fingerprint(current),
        parse_lexical_storage_fingerprint(saved),
    ) {
        (Some(current), Some(saved)) => {
            current.db_len == saved.db_len
                && current.wal_len == saved.wal_len
                && current.db_mtime_ms.abs_diff(saved.db_mtime_ms)
                    <= u64::try_from(LEXICAL_STORAGE_FINGERPRINT_MTIME_TOLERANCE_MS)
                        .unwrap_or(u64::MAX)
                && current.wal_mtime_ms.abs_diff(saved.wal_mtime_ms)
                    <= u64::try_from(LEXICAL_STORAGE_FINGERPRINT_MTIME_TOLERANCE_MS)
                        .unwrap_or(u64::MAX)
        }
        _ => current == saved,
    }
}

pub(crate) fn inspect_search_assets(
    input: InspectSearchAssetsInput<'_>,
) -> Result<SearchAssetSnapshot> {
    let InspectSearchAssetsInput {
        data_dir,
        db_path,
        stale_threshold,
        last_indexed_at_ms,
        now_ms,
        maintenance,
        semantic_preference,
        db_available,
        compute_lexical_fingerprint,
        inspect_semantic,
    } = input;

    let lexical = inspect_lexical_assets(InspectLexicalAssetsInput {
        data_dir,
        db_path,
        stale_threshold,
        last_indexed_at_ms,
        now_ms,
        maintenance,
        db_available,
        compute_lexical_fingerprint,
    })?;
    let current_db_fingerprint = lexical.fingerprint.current_db_fingerprint.as_deref();
    let semantic = if inspect_semantic {
        inspect_semantic_assets(
            data_dir,
            db_path,
            semantic_preference,
            current_db_fingerprint,
            db_available,
        )
    } else {
        semantic_state_not_inspected(data_dir, semantic_preference, current_db_fingerprint)
    };

    Ok(SearchAssetSnapshot { lexical, semantic })
}

fn semantic_state_not_inspected(
    data_dir: &Path,
    preference: SemanticPreference,
    current_db_fingerprint: Option<&str>,
) -> SemanticAssetState {
    let (fast_tier, quality_tier, backlog, checkpoint) =
        semantic_manifest_progress(data_dir, current_db_fingerprint);
    let preference_surface = semantic_preference_surface(data_dir, preference);

    SemanticAssetState {
        status: "not_inspected",
        availability: "not_inspected",
        summary: "semantic assets were not inspected for this fast path".to_string(),
        available: false,
        can_search: false,
        fallback_mode: Some("lexical"),
        preferred_backend: preference_surface.preferred_backend,
        embedder_id: None,
        vector_index_path: None,
        model_dir: preference_surface.model_dir,
        hnsw_path: None,
        hnsw_ready: false,
        progressive_ready: semantic_progressive_assets_ready(data_dir),
        // The fast-path skip-DB-open lane doesn't have an
        // `availability` to consult, so we can't honestly call the
        // tiers queryable here — leave the sub-fix-3 flags false. The
        // caller upgrades to `semantic_state_from_availability` when
        // it needs an answer.
        quality_tier_published: false,
        semantic_only_search_available: false,
        hint: Some(
            "Use 'cass status --json' or 'cass models status --json' for semantic readiness."
                .to_string(),
        ),
        fast_tier,
        quality_tier,
        backlog,
        checkpoint,
    }
}

pub(crate) fn inspect_semantic_assets(
    data_dir: &Path,
    db_path: &Path,
    preference: SemanticPreference,
    current_db_fingerprint: Option<&str>,
    db_available: bool,
) -> SemanticAssetState {
    if !db_available {
        let availability = SemanticAvailability::DatabaseUnavailable {
            db_path: db_path.to_path_buf(),
            error: "database unavailable during asset inspection".to_string(),
        };
        return semantic_state_from_availability(
            data_dir,
            &availability,
            preference,
            current_db_fingerprint,
        );
    }

    let availability = match preference {
        SemanticPreference::DefaultModel => probe_semantic_availability(data_dir),
        SemanticPreference::HashFallback => probe_hash_semantic_availability(data_dir),
    };
    semantic_state_from_availability(data_dir, &availability, preference, current_db_fingerprint)
}

pub(crate) fn semantic_state_from_availability(
    data_dir: &Path,
    availability: &SemanticAvailability,
    preference: SemanticPreference,
    current_db_fingerprint: Option<&str>,
) -> SemanticAssetState {
    let (mut fast_tier, mut quality_tier, backlog, checkpoint) =
        semantic_manifest_progress(data_dir, current_db_fingerprint);
    let preference_surface = semantic_preference_surface(data_dir, preference);
    let base_embedder_id = semantic_embedder_id(availability, preference);
    if let (Some(db_fingerprint), Some(embedder_id)) =
        (current_db_fingerprint, base_embedder_id.as_deref())
    {
        promote_complete_shard_generation_state(
            data_dir,
            TierKind::Fast,
            embedder_id,
            db_fingerprint,
            &mut fast_tier,
        );
        promote_complete_shard_generation_state(
            data_dir,
            TierKind::Quality,
            embedder_id,
            db_fingerprint,
            &mut quality_tier,
        );
    }
    let base_vector_index_path = semantic_vector_index_path(data_dir, availability, preference);
    let base_model_dir = preference_surface.model_dir;
    let base_hnsw_path = base_embedder_id
        .as_deref()
        .map(|embedder_id| hnsw_index_path(data_dir, embedder_id));
    let runtime = semantic_runtime_surface(SemanticRuntimeInputs {
        data_dir,
        availability,
        preference,
        fast_tier: &fast_tier,
        quality_tier: &quality_tier,
        backlog: &backlog,
        checkpoint: &checkpoint,
        base_embedder_id: base_embedder_id.clone(),
        base_vector_index_path: base_vector_index_path.clone(),
        base_model_dir: base_model_dir.clone(),
        base_hnsw_path: base_hnsw_path.clone(),
    });
    let use_runtime_paths = runtime.embedder_id.is_some();
    let embedder_id = runtime.embedder_id.or(base_embedder_id);
    let vector_index_path = if use_runtime_paths {
        runtime.vector_index_path
    } else {
        runtime.vector_index_path.or(base_vector_index_path)
    };
    let model_dir = if use_runtime_paths {
        runtime.model_dir
    } else {
        runtime.model_dir.or(base_model_dir)
    };
    let hnsw_path = if use_runtime_paths {
        runtime.hnsw_path
    } else {
        runtime.hnsw_path.or(base_hnsw_path)
    };
    let hnsw_ready = hnsw_path.as_ref().is_some_and(|path| path.is_file());
    let progressive_ready = semantic_progressive_assets_ready(data_dir);

    // Sub-fix 3 for cass#257: report quality-tier readiness as a
    // first-class flag so operators querying `--mode semantic` can
    // tell when the quality index is usable even while the
    // progressive/hybrid stack remains incomplete.
    let quality_tier_published = semantic_tier_queryable(availability, &quality_tier);
    let fast_tier_queryable = semantic_tier_queryable(availability, &fast_tier);
    let semantic_only_search_available = quality_tier_published || fast_tier_queryable;

    SemanticAssetState {
        status: runtime.status,
        availability: runtime.availability,
        summary: runtime.summary,
        available: runtime.can_search,
        can_search: runtime.can_search,
        fallback_mode: runtime.fallback_mode,
        preferred_backend: preference_surface.preferred_backend,
        embedder_id,
        vector_index_path,
        model_dir,
        hnsw_path,
        hnsw_ready,
        progressive_ready,
        quality_tier_published,
        semantic_only_search_available,
        hint: runtime.hint,
        fast_tier,
        quality_tier,
        backlog,
        checkpoint,
    }
}

fn semantic_preference_surface(
    data_dir: &Path,
    preference: SemanticPreference,
) -> SemanticPreferenceSurface {
    match preference {
        SemanticPreference::DefaultModel => SemanticPreferenceSurface {
            preferred_backend: "fastembed",
            model_dir: active_policy_model_dir(data_dir),
        },
        SemanticPreference::HashFallback => SemanticPreferenceSurface {
            preferred_backend: "hash",
            model_dir: None,
        },
    }
}

fn semantic_runtime_surface(inputs: SemanticRuntimeInputs<'_>) -> SemanticRuntimeSurface {
    let SemanticRuntimeInputs {
        data_dir,
        availability,
        preference,
        fast_tier,
        quality_tier,
        backlog,
        checkpoint,
        base_embedder_id,
        base_vector_index_path,
        base_model_dir,
        base_hnsw_path,
    } = inputs;
    let base_status = semantic_status_from_availability(availability);
    let base_availability = semantic_availability_code(availability);
    let base_summary = availability.summary();
    let base_can_search = availability.can_search();
    let base_hint = semantic_hint(availability, preference);

    if matches!(
        availability,
        SemanticAvailability::Disabled { .. }
            | SemanticAvailability::DatabaseUnavailable { .. }
            | SemanticAvailability::LoadFailed { .. }
    ) {
        return SemanticRuntimeSurface {
            status: base_status,
            availability: base_availability,
            summary: base_summary,
            can_search: base_can_search,
            fallback_mode: (!base_can_search).then_some("lexical"),
            hint: base_hint,
            embedder_id: base_embedder_id,
            vector_index_path: base_vector_index_path,
            model_dir: base_model_dir,
            hnsw_path: base_hnsw_path,
        };
    }

    let quality_queryable = semantic_tier_queryable(availability, quality_tier);
    let fast_queryable = semantic_tier_queryable(availability, fast_tier);
    let checkpoint_active = checkpoint.active;
    let backlog_pending = backlog.pending_work;
    let manifest_assets_present = fast_tier.present || quality_tier.present;
    let backfill_active = checkpoint_active || backlog_pending;

    let effective_embedder_id = if quality_queryable {
        quality_tier.embedder_id.clone()
    } else if fast_queryable {
        fast_tier.embedder_id.clone()
    } else {
        None
    };
    let effective_vector_index_path = if quality_queryable {
        quality_tier.index_path.clone()
    } else if fast_queryable {
        fast_tier.index_path.clone()
    } else {
        None
    }
    .or_else(|| {
        effective_embedder_id
            .as_deref()
            .map(|embedder_id| vector_index_path(data_dir, embedder_id))
    });
    let effective_model_dir = effective_embedder_id.as_deref().and_then(|embedder_id| {
        (!semantic_embedder_is_hash(embedder_id))
            .then(|| model_dir_for_embedder_id(data_dir, embedder_id))
            .flatten()
    });
    let effective_hnsw_path = effective_embedder_id
        .as_deref()
        .map(|embedder_id| hnsw_index_path(data_dir, embedder_id));

    if quality_queryable || fast_queryable {
        let fully_ready = (quality_queryable || fast_queryable) && !backfill_active;
        let summary = if quality_queryable && backfill_active {
            "semantic quality tier is usable; residual semantic backfill is still finishing"
                .to_string()
        } else if quality_queryable {
            "semantic quality tier ready".to_string()
        } else if backfill_active {
            "semantic fast tier is usable; higher-quality semantic backfill is still in progress"
                .to_string()
        } else {
            "semantic fast tier ready".to_string()
        };
        let hint = if backfill_active {
            Some(
                "Semantic refinement is already usable; continue searching while higher-quality backfill finishes."
                    .to_string(),
            )
        } else {
            None
        };
        return SemanticRuntimeSurface {
            status: if fully_ready { "ready" } else { "building" },
            availability: if fully_ready {
                "ready"
            } else {
                "index_building"
            },
            summary,
            can_search: true,
            fallback_mode: None,
            hint,
            embedder_id: effective_embedder_id,
            vector_index_path: effective_vector_index_path,
            model_dir: effective_model_dir,
            hnsw_path: effective_hnsw_path,
        };
    }

    if backfill_active {
        return SemanticRuntimeSurface {
            status: "building",
            availability: "index_building",
            summary: "semantic backfill is in progress for the current database".to_string(),
            can_search: false,
            fallback_mode: Some("lexical"),
            hint: Some(
                "Run 'cass index --semantic' to finish backfilling current semantic assets; search will use lexical fallback until then."
                    .to_string(),
            ),
            embedder_id: base_embedder_id,
            vector_index_path: base_vector_index_path,
            model_dir: base_model_dir,
            hnsw_path: base_hnsw_path,
        };
    }

    if manifest_assets_present {
        return SemanticRuntimeSurface {
            status: "stale",
            availability: "update_available",
            summary: "semantic artifacts exist but do not match the current database".to_string(),
            can_search: false,
            fallback_mode: Some("lexical"),
            hint: Some(
                "Run 'cass index --semantic' to refresh semantic assets for the current database; search will use lexical fallback until then."
                    .to_string(),
            ),
            embedder_id: base_embedder_id,
            vector_index_path: base_vector_index_path,
            model_dir: base_model_dir,
            hnsw_path: base_hnsw_path,
        };
    }

    SemanticRuntimeSurface {
        status: base_status,
        availability: base_availability,
        summary: base_summary,
        can_search: base_can_search,
        fallback_mode: (!base_can_search).then_some("lexical"),
        hint: base_hint,
        embedder_id: base_embedder_id,
        vector_index_path: base_vector_index_path,
        model_dir: base_model_dir,
        hnsw_path: base_hnsw_path,
    }
}

fn active_policy_model_dir(data_dir: &Path) -> Option<PathBuf> {
    let policy = SemanticPolicy::resolve(&CliSemanticOverrides::default());
    let embedder_name = FastEmbedder::canonical_name(&policy.quality_tier_embedder)?;
    FastEmbedder::runtime_model_dir_for(data_dir, embedder_name)
}

fn model_dir_for_embedder_id(data_dir: &Path, embedder_id: &str) -> Option<PathBuf> {
    let embedder_name = FastEmbedder::canonical_name(embedder_id)?;
    FastEmbedder::runtime_model_dir_for(data_dir, embedder_name)
}

fn semantic_tier_queryable(
    availability: &SemanticAvailability,
    tier: &SemanticTierAssetState,
) -> bool {
    if !tier.ready || tier.current_db_matches == Some(false) {
        return false;
    }
    let Some(embedder_id) = tier.embedder_id.as_deref() else {
        return false;
    };
    if semantic_embedder_is_hash(embedder_id) {
        !matches!(
            availability,
            SemanticAvailability::Disabled { .. }
                | SemanticAvailability::DatabaseUnavailable { .. }
                | SemanticAvailability::LoadFailed { .. }
        )
    } else {
        matches!(
            availability,
            SemanticAvailability::Ready { .. }
                | SemanticAvailability::UpdateAvailable { .. }
                | SemanticAvailability::IndexBuilding { .. }
                | SemanticAvailability::IndexMissing { .. }
        )
    }
}

fn semantic_embedder_is_hash(embedder_id: &str) -> bool {
    embedder_id == HashEmbedder::default().id()
}

fn semantic_manifest_progress(
    data_dir: &Path,
    current_db_fingerprint: Option<&str>,
) -> (
    SemanticTierAssetState,
    SemanticTierAssetState,
    SemanticBacklogProgressState,
    SemanticCheckpointProgressState,
) {
    let manifest = SemanticManifest::load_or_default(data_dir).unwrap_or_default();
    let fast_tier = semantic_tier_asset_state(manifest.fast_tier.as_ref(), current_db_fingerprint);
    let quality_tier =
        semantic_tier_asset_state(manifest.quality_tier.as_ref(), current_db_fingerprint);
    let backlog = semantic_backlog_progress_state(&manifest, current_db_fingerprint);
    let checkpoint =
        semantic_checkpoint_progress_state(manifest.checkpoint.as_ref(), current_db_fingerprint);
    (fast_tier, quality_tier, backlog, checkpoint)
}

fn semantic_tier_asset_state(
    artifact: Option<&ArtifactRecord>,
    current_db_fingerprint: Option<&str>,
) -> SemanticTierAssetState {
    let Some(artifact) = artifact else {
        return SemanticTierAssetState::default();
    };

    SemanticTierAssetState {
        present: true,
        ready: artifact.ready,
        current_db_matches: current_db_fingerprint.map(|fp| artifact.db_fingerprint == fp),
        conversation_count: Some(artifact.conversation_count),
        doc_count: Some(artifact.doc_count),
        embedder_id: Some(artifact.embedder_id.clone()),
        model_revision: Some(artifact.model_revision.clone()),
        completed_at_ms: Some(artifact.completed_at_ms),
        size_bytes: Some(artifact.size_bytes),
        index_path: None,
    }
}

fn resolve_semantic_artifact_path(data_dir: &Path, recorded_path: &str) -> Option<PathBuf> {
    semantic_shard_artifact_path_is_safe(recorded_path).then(|| data_dir.join(recorded_path))
}

fn complete_shard_records_for_state(
    data_dir: &Path,
    tier: TierKind,
    embedder_id: &str,
    db_fingerprint: &str,
) -> Option<Vec<SemanticShardRecord>> {
    let manifest = SemanticShardManifest::load(data_dir).ok().flatten()?;
    let summary = manifest.summary(tier, embedder_id, db_fingerprint);
    if !summary.complete {
        return None;
    }
    let mut records = manifest
        .shards
        .into_iter()
        .filter(|shard| shard.matches_generation(tier, embedder_id, db_fingerprint))
        .collect::<Vec<_>>();
    records.sort_by_key(|shard| shard.shard_index);
    if records.len() != usize::try_from(summary.shard_count).unwrap_or(usize::MAX) {
        return None;
    }
    let first = records.first()?;
    for (expected_index, shard) in records.iter().enumerate() {
        if shard.shard_index != u32::try_from(expected_index).unwrap_or(u32::MAX)
            || !shard.ready
            || !shard.mmap_ready
            || shard.model_revision != first.model_revision
            || shard.schema_version != SEMANTIC_SCHEMA_VERSION
            || shard.chunking_version != CHUNKING_STRATEGY_VERSION
            || shard.dimension == 0
            || shard.dimension != first.dimension
            || shard.total_conversations != first.total_conversations
        {
            return None;
        }
        let artifact_path = resolve_semantic_artifact_path(data_dir, &shard.index_path)?;
        if !artifact_path.is_file() {
            return None;
        }
    }
    Some(records)
}

fn promote_complete_shard_generation_state(
    data_dir: &Path,
    tier: TierKind,
    embedder_id: &str,
    db_fingerprint: &str,
    state: &mut SemanticTierAssetState,
) {
    if state.ready && state.current_db_matches == Some(true) {
        return;
    }
    let Some(records) =
        complete_shard_records_for_state(data_dir, tier, embedder_id, db_fingerprint)
    else {
        return;
    };
    let doc_count = records
        .iter()
        .map(|shard| shard.doc_count)
        .fold(0, u64::saturating_add);
    let size_bytes = records
        .iter()
        .map(|shard| shard.size_bytes)
        .fold(0, u64::saturating_add);
    let completed_at_ms = records
        .iter()
        .map(|shard| shard.completed_at_ms)
        .max()
        .unwrap_or(0);
    let first = &records[0];
    let Some(first_index_path) = resolve_semantic_artifact_path(data_dir, &first.index_path) else {
        return;
    };
    *state = SemanticTierAssetState {
        present: true,
        ready: true,
        current_db_matches: Some(true),
        conversation_count: Some(first.total_conversations),
        doc_count: Some(doc_count),
        embedder_id: Some(first.embedder_id.clone()),
        model_revision: Some(first.model_revision.clone()),
        completed_at_ms: Some(completed_at_ms),
        size_bytes: Some(size_bytes),
        index_path: Some(first_index_path),
    };
}

fn semantic_backlog_progress_state(
    manifest: &SemanticManifest,
    current_db_fingerprint: Option<&str>,
) -> SemanticBacklogProgressState {
    let backlog = &manifest.backlog;
    let current_db_matches = current_db_fingerprint.and_then(|fp| {
        (backlog.computed_at_ms > 0 || !backlog.db_fingerprint.is_empty())
            .then(|| backlog.is_current(fp))
    });

    SemanticBacklogProgressState {
        total_conversations: backlog.total_conversations,
        fast_tier_processed: backlog.fast_tier_processed,
        fast_tier_remaining: backlog.fast_tier_remaining(),
        quality_tier_processed: backlog.quality_tier_processed,
        quality_tier_remaining: backlog.quality_tier_remaining(),
        pending_work: backlog.has_pending_work() || manifest.checkpoint.is_some(),
        current_db_matches,
        computed_at_ms: (backlog.computed_at_ms > 0).then_some(backlog.computed_at_ms),
    }
}

fn semantic_checkpoint_progress_state(
    checkpoint: Option<&BuildCheckpoint>,
    current_db_fingerprint: Option<&str>,
) -> SemanticCheckpointProgressState {
    let Some(checkpoint) = checkpoint else {
        return SemanticCheckpointProgressState::default();
    };

    SemanticCheckpointProgressState {
        active: true,
        tier: Some(checkpoint.tier.as_str()),
        current_db_matches: current_db_fingerprint.map(|fp| checkpoint.is_valid(fp)),
        completed: Some(checkpoint.is_complete()),
        conversations_processed: Some(checkpoint.conversations_processed),
        total_conversations: Some(checkpoint.total_conversations),
        progress_pct: Some(checkpoint.progress_pct()),
        docs_embedded: Some(checkpoint.docs_embedded),
        last_offset: Some(checkpoint.last_offset),
        saved_at_ms: Some(checkpoint.saved_at_ms),
    }
}

struct InspectLexicalAssetsInput<'a> {
    data_dir: &'a Path,
    db_path: &'a Path,
    stale_threshold: u64,
    last_indexed_at_ms: Option<i64>,
    /// F4 (cass tech debt): full-precision wall clock; the legacy
    /// `now_secs` field was widening the second-precision comparison
    /// against ms-precision `last_progress_at_ms`. Derive `now_secs`
    /// locally from this when needed for age math.
    now_ms: i64,
    maintenance: SearchMaintenanceSnapshot,
    db_available: bool,
    compute_lexical_fingerprint: bool,
}

fn inspect_lexical_assets(input: InspectLexicalAssetsInput<'_>) -> Result<LexicalAssetState> {
    let InspectLexicalAssetsInput {
        data_dir,
        db_path,
        stale_threshold,
        last_indexed_at_ms,
        now_ms,
        maintenance,
        db_available,
        compute_lexical_fingerprint,
    } = input;
    let index_path = crate::search::tantivy::expected_index_dir(data_dir);
    let checkpoint = load_lexical_rebuild_checkpoint(&index_path)
        .with_context(|| format!("loading lexical checkpoint from {}", index_path.display()))?;
    let current_db_fingerprint = if db_available && compute_lexical_fingerprint {
        Some(
            lexical_storage_fingerprint_for_db(db_path).with_context(|| {
                format!(
                    "computing lexical storage fingerprint for {}",
                    db_path.display()
                )
            })?,
        )
    } else {
        None
    };

    Ok(lexical_state_from_observations(LexicalObservationInput {
        index_path: &index_path,
        db_path,
        stale_threshold,
        last_indexed_at_ms,
        now_ms,
        maintenance,
        checkpoint: checkpoint.as_ref(),
        current_db_fingerprint: current_db_fingerprint.as_deref(),
    }))
}

struct LexicalObservationInput<'a> {
    index_path: &'a Path,
    db_path: &'a Path,
    stale_threshold: u64,
    last_indexed_at_ms: Option<i64>,
    /// Full-precision wall clock (F4); see [`InspectSearchAssetsInput::now_ms`].
    now_ms: i64,
    maintenance: SearchMaintenanceSnapshot,
    checkpoint: Option<&'a LexicalRebuildCheckpoint>,
    current_db_fingerprint: Option<&'a str>,
}

fn lexical_state_from_observations(input: LexicalObservationInput<'_>) -> LexicalAssetState {
    let LexicalObservationInput {
        index_path,
        db_path,
        stale_threshold,
        last_indexed_at_ms,
        now_ms,
        maintenance,
        checkpoint,
        current_db_fingerprint,
    } = input;
    let exists = crate::search::tantivy::searchable_index_exists(index_path);
    let checkpoint_db_matches =
        checkpoint.map(|state| crate::stored_path_identity_matches(&state.db_path, db_path));
    let schema_matches = checkpoint.map(|state| state.schema_hash == SCHEMA_HASH);
    let page_size_matches =
        checkpoint.map(|state| state.page_size == LEXICAL_REBUILD_PAGE_SIZE_PUBLIC);
    let page_size_compatible =
        checkpoint.map(|state| lexical_rebuild_page_size_is_compatible(state.page_size));
    let checkpoint_fingerprint = checkpoint.map(|state| state.storage_fingerprint.as_str());
    let fingerprint_matches = match (current_db_fingerprint, checkpoint_fingerprint) {
        (Some(current), Some(saved)) => Some(lexical_storage_fingerprints_match(current, saved)),
        _ => None,
    };
    let checkpoint_incomplete = checkpoint.is_some_and(|state| !state.completed);
    let checkpoint_db_mismatch = checkpoint_db_matches == Some(false);
    let contract_mismatch = schema_matches == Some(false) || page_size_compatible == Some(false);
    let fingerprint_mismatch = fingerprint_matches == Some(false);
    // F4 (cass tech debt): derive the (legacy) second-resolution clock
    // from `now_ms` rather than the other way around so the comparison
    // against ms-precision `last_progress_at_ms` below is no longer
    // forced into second-bin alignment. `as u64` is correct because
    // wall-clock millis fits well inside i63 (until year ~292477).
    let now_secs: u64 = now_ms.div_euclid(1000).max(0) as u64;
    let age_seconds = last_indexed_at_ms
        .and_then(|ts| (ts > 0).then(|| now_secs.saturating_sub((ts / 1000) as u64)));
    let age_stale = match age_seconds {
        Some(age) => age > stale_threshold,
        None => true,
    };
    let maintenance_targets_current_db = maintenance
        .db_path
        .as_ref()
        .is_none_or(|lock_db_path| crate::path_identities_match(lock_db_path, db_path));
    let watch_active = maintenance.active
        && maintenance_targets_current_db
        && maintenance
            .mode
            .is_some_and(SearchMaintenanceMode::watch_active);
    let rebuilding = maintenance.active
        && maintenance_targets_current_db
        && maintenance
            .mode
            .is_some_and(SearchMaintenanceMode::rebuild_active);
    let active_rebuild_progress = rebuilding;
    // Forward-progress liveness check (issue #258): when a rebuild is
    // active but the indexing thread has not posted progress within
    // `CASS_REBUILD_STALL_DETECT_SECS` (default 120 s), report
    // `stalled` rather than `rebuilding`. The lock file's
    // `updated_at_ms` is heartbeat-refreshed every ~1 s by a separate
    // thread, so it cannot be used as a "work is happening" signal —
    // only the indexer-thread-owned `last_progress_at_ms` can.
    //
    // `now_ms` is now passed in at full ms precision (F4); the old
    // shape derived it from `now_secs` and quantised the comparison.
    let stall_age_ms = if rebuilding && maintenance_targets_current_db {
        maintenance_stall_age_ms(&maintenance, now_ms)
    } else {
        None
    };
    let stalled = stall_age_ms.is_some();
    let last_progress_at_ms = maintenance
        .last_progress_at_ms
        .filter(|_| maintenance_targets_current_db);
    let last_progress_age_ms = last_progress_at_ms
        .filter(|_| rebuilding)
        .map(|ts| now_ms.saturating_sub(ts));
    let stale = if rebuilding {
        // A stalled rebuild leaves the on-disk index unchanged; if it
        // existed before the stall it is still searchable, so treat
        // `stalled` like the indexer just hadn't gotten there yet.
        !exists || contract_mismatch
    } else {
        exists
            && (age_stale
                || checkpoint_db_mismatch
                || checkpoint_incomplete
                || contract_mismatch
                || fingerprint_mismatch)
    };
    let fresh = exists && !stale && !rebuilding;
    let status = if stalled {
        "stalled"
    } else if rebuilding {
        "building"
    } else if !exists {
        "missing"
    } else if stale {
        "stale"
    } else {
        "ready"
    };
    let status_reason = if stalled {
        let secs = stall_age_ms.unwrap_or(0) / 1000;
        Some(format!(
            "indexing thread has not posted forward progress for {secs}s while the lock heartbeat keeps refreshing — see issue #258 for diagnostics (run `cass doctor check --json` and capture a stack trace)"
        ))
    } else if rebuilding {
        Some("lexical rebuild is in progress".to_string())
    } else if !exists {
        Some("lexical Tantivy metadata missing".to_string())
    } else if checkpoint_db_mismatch {
        Some("lexical rebuild checkpoint points at a different database".to_string())
    } else if contract_mismatch {
        Some("lexical rebuild checkpoint no longer matches the active lexical contract".to_string())
    } else if fingerprint_mismatch {
        Some("database fingerprint changed since the last lexical checkpoint".to_string())
    } else if checkpoint_incomplete {
        Some("lexical rebuild checkpoint is incomplete".to_string())
    } else if age_stale {
        Some("lexical index is older than the stale threshold".to_string())
    } else {
        None
    };
    let checkpoint_progress_usable = checkpoint.is_some()
        && checkpoint_db_matches == Some(true)
        && schema_matches == Some(true)
        && page_size_compatible == Some(true)
        && if active_rebuild_progress {
            true
        } else {
            current_db_fingerprint.is_some() && fingerprint_matches == Some(true)
        };
    let pending_sessions = checkpoint
        .filter(|_| checkpoint_progress_usable)
        .map(|state| {
            state
                .total_conversations
                .saturating_sub(state.processed_conversations) as u64
        })
        .unwrap_or(0);
    let maintenance_activity_at_ms = maintenance_targets_current_db
        .then_some(())
        .and(maintenance.updated_at_ms.or(maintenance.started_at_ms));
    let checkpoint_activity_at_ms = checkpoint
        .filter(|_| checkpoint_progress_usable)
        .and_then(|state| (state.updated_at_ms > 0).then_some(state.updated_at_ms));
    let activity_at_ms = match (checkpoint_activity_at_ms, maintenance_activity_at_ms) {
        (Some(checkpoint_ts), Some(maintenance_ts)) => Some(checkpoint_ts.max(maintenance_ts)),
        (Some(checkpoint_ts), None) => Some(checkpoint_ts),
        (None, Some(maintenance_ts)) => Some(maintenance_ts),
        (None, None) => None,
    };

    LexicalAssetState {
        status,
        exists,
        fresh,
        stale,
        rebuilding,
        stalled,
        last_progress_age_ms,
        last_progress_at_ms,
        watch_active,
        last_indexed_at_ms,
        age_seconds,
        stale_threshold_seconds: stale_threshold,
        activity_at_ms,
        pending_sessions,
        processed_conversations: checkpoint
            .filter(|_| checkpoint_progress_usable)
            .map(|state| state.processed_conversations as u64),
        total_conversations: checkpoint
            .filter(|_| checkpoint_progress_usable)
            .map(|state| state.total_conversations as u64),
        indexed_docs: checkpoint
            .filter(|_| checkpoint_progress_usable)
            .map(|state| state.indexed_docs as u64),
        status_reason,
        fingerprint: LexicalFingerprintState {
            current_db_fingerprint: current_db_fingerprint.map(ToOwned::to_owned),
            checkpoint_fingerprint: checkpoint.map(|state| state.storage_fingerprint.clone()),
            matches_current_db_fingerprint: fingerprint_matches,
        },
        checkpoint: LexicalCheckpointState {
            present: checkpoint.is_some(),
            completed: checkpoint.map(|state| state.completed),
            db_matches: checkpoint_db_matches,
            schema_matches,
            page_size_matches,
            page_size_compatible,
        },
    }
}

fn semantic_embedder_id(
    availability: &SemanticAvailability,
    preference: SemanticPreference,
) -> Option<String> {
    match availability {
        SemanticAvailability::Ready { embedder_id }
        | SemanticAvailability::UpdateAvailable { embedder_id, .. }
        | SemanticAvailability::IndexBuilding { embedder_id, .. } => Some(embedder_id.clone()),
        SemanticAvailability::HashFallback => Some(HashEmbedder::default().id().to_string()),
        _ => match preference {
            SemanticPreference::DefaultModel => {
                Some(FastEmbedder::embedder_id_static().to_string())
            }
            SemanticPreference::HashFallback => Some(HashEmbedder::default().id().to_string()),
        },
    }
}

fn semantic_vector_index_path(
    data_dir: &Path,
    availability: &SemanticAvailability,
    preference: SemanticPreference,
) -> Option<PathBuf> {
    match availability {
        SemanticAvailability::IndexMissing { index_path } => Some(index_path.clone()),
        _ => semantic_embedder_id(availability, preference)
            .map(|embedder_id| vector_index_path(data_dir, &embedder_id)),
    }
}

fn semantic_progressive_assets_ready(data_dir: &Path) -> bool {
    let vector_dir = data_dir.join(VECTOR_INDEX_DIR);
    vector_dir.join("vector.fast.idx").is_file() && vector_dir.join("vector.quality.idx").is_file()
}

fn semantic_availability_code(availability: &SemanticAvailability) -> &'static str {
    match availability {
        SemanticAvailability::Ready { .. } => "ready",
        SemanticAvailability::NotInstalled => "not_installed",
        SemanticAvailability::NeedsConsent => "needs_consent",
        SemanticAvailability::Downloading { .. } => "downloading",
        SemanticAvailability::Verifying => "verifying",
        SemanticAvailability::IndexBuilding { .. } => "index_building",
        SemanticAvailability::HashFallback => "hash_fallback",
        SemanticAvailability::Disabled { .. } => "disabled",
        SemanticAvailability::ModelMissing { .. } => "model_missing",
        SemanticAvailability::IndexMissing { .. } => "index_missing",
        SemanticAvailability::DatabaseUnavailable { .. } => "database_unavailable",
        SemanticAvailability::LoadFailed { .. } => "load_failed",
        SemanticAvailability::UpdateAvailable { .. } => "update_available",
    }
}

fn semantic_status_from_availability(availability: &SemanticAvailability) -> &'static str {
    match availability {
        SemanticAvailability::Ready { .. } => "ready",
        SemanticAvailability::HashFallback => "hash_fallback",
        SemanticAvailability::Downloading { .. }
        | SemanticAvailability::Verifying
        | SemanticAvailability::IndexBuilding { .. } => "building",
        SemanticAvailability::Disabled { .. } => "disabled",
        SemanticAvailability::UpdateAvailable { .. } => "stale",
        SemanticAvailability::NotInstalled
        | SemanticAvailability::NeedsConsent
        | SemanticAvailability::ModelMissing { .. }
        | SemanticAvailability::IndexMissing { .. } => "missing",
        SemanticAvailability::DatabaseUnavailable { .. }
        | SemanticAvailability::LoadFailed { .. } => "error",
    }
}

fn semantic_hint(
    availability: &SemanticAvailability,
    preference: SemanticPreference,
) -> Option<String> {
    let hint = match (preference, availability) {
        (SemanticPreference::HashFallback, SemanticAvailability::IndexMissing { .. }) => {
            "Run 'cass index --semantic --embedder hash' to build the hash vector index; lexical search remains available without semantic assets"
        }
        (SemanticPreference::HashFallback, SemanticAvailability::LoadFailed { .. })
        | (SemanticPreference::HashFallback, SemanticAvailability::DatabaseUnavailable { .. }) => {
            "Run 'cass index --semantic --embedder hash' after the database is healthy; lexical search remains available"
        }
        (SemanticPreference::HashFallback, _) => {
            "Run 'cass index --semantic --embedder hash' to build the hash vector index; lexical search remains available"
        }
        (_, SemanticAvailability::NotInstalled)
        | (_, SemanticAvailability::NeedsConsent)
        | (_, SemanticAvailability::ModelMissing { .. }) => {
            "Run 'cass models install' and then 'cass index --semantic'; lexical search remains available without the model"
        }
        (_, SemanticAvailability::IndexMissing { .. })
        | (_, SemanticAvailability::UpdateAvailable { .. })
        | (_, SemanticAvailability::IndexBuilding { .. }) => {
            "Run 'cass index --semantic' to build or refresh vector assets; lexical search remains available"
        }
        (_, SemanticAvailability::Downloading { .. }) | (_, SemanticAvailability::Verifying) => {
            "Wait for the semantic model installation to finish; lexical search remains available"
        }
        (_, SemanticAvailability::Disabled { .. }) => {
            "Semantic search is disabled by policy; lexical search remains available, or re-enable semantic search"
        }
        (_, SemanticAvailability::DatabaseUnavailable { .. })
        | (_, SemanticAvailability::LoadFailed { .. }) => {
            "Restore the semantic assets and database; lexical search remains available when the archive database is healthy"
        }
        (_, SemanticAvailability::Ready { .. }) | (_, SemanticAvailability::HashFallback) => {
            return None;
        }
    };
    Some(hint.to_string())
}

// ---------------------------------------------------------------------------
// Maintenance coordination: single-flight, attach-to-progress, fail-open
// ---------------------------------------------------------------------------

#[cfg_attr(not(test), allow(dead_code))]
const HEARTBEAT_STALE_THRESHOLD_MS: i64 = 30_000;
#[cfg_attr(not(test), allow(dead_code))]
const BOUNDED_WAIT_DEFAULT: Duration = Duration::from_secs(5);
#[cfg_attr(not(test), allow(dead_code))]
const POLL_INTERVAL_DEFAULT: Duration = Duration::from_millis(250);

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) enum MaintenanceCoordinationOutcome {
    Idle,
    Active {
        job_id: String,
        job_kind: SearchMaintenanceJobKind,
        phase: Option<String>,
        started_at_ms: i64,
        updated_at_ms: i64,
    },
    Stale {
        job_id: String,
        reason: String,
    },
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) enum MaintenanceDecision {
    Launch,
    AttachOrWait {
        job_id: String,
        job_kind: SearchMaintenanceJobKind,
        phase: Option<String>,
        elapsed_ms: u64,
    },
    FailOpen {
        reason: String,
    },
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn evaluate_maintenance_coordination(
    data_dir: &Path,
    now_ms: i64,
) -> MaintenanceCoordinationOutcome {
    evaluate_maintenance_coordination_from_snapshot(
        &read_search_maintenance_snapshot(data_dir),
        now_ms,
    )
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn evaluate_maintenance_coordination_from_snapshot(
    snapshot: &SearchMaintenanceSnapshot,
    now_ms: i64,
) -> MaintenanceCoordinationOutcome {
    if !snapshot.active {
        return MaintenanceCoordinationOutcome::Idle;
    }
    let job_id = maintenance_snapshot_job_id(snapshot);
    if let Some(updated_at_ms) = snapshot.updated_at_ms {
        let heartbeat_age_ms = now_ms.saturating_sub(updated_at_ms);
        if heartbeat_age_ms > HEARTBEAT_STALE_THRESHOLD_MS {
            return MaintenanceCoordinationOutcome::Stale {
                job_id,
                reason: format!(
                    "heartbeat is {heartbeat_age_ms}ms old (threshold {HEARTBEAT_STALE_THRESHOLD_MS}ms)"
                ),
            };
        }
    }
    // Forward-progress liveness check (issue #258). Even if the
    // heartbeat is fresh, treat the job as `Stale` when the indexing
    // thread itself has not posted progress for longer than the stall
    // threshold. Coordination consumers (search fail-open, attach-or-
    // wait) then route around the wedged worker instead of waiting on
    // it indefinitely.
    if let Some(stall_age_ms) = maintenance_stall_age_ms(snapshot, now_ms) {
        let threshold_ms = rebuild_stall_detect_threshold_ms().unwrap_or(0);
        return MaintenanceCoordinationOutcome::Stale {
            job_id,
            reason: format!(
                "indexing thread has not posted forward progress for {stall_age_ms}ms while the heartbeat keeps refreshing (stall threshold {threshold_ms}ms) — see issue #258"
            ),
        };
    }
    MaintenanceCoordinationOutcome::Active {
        job_id,
        job_kind: snapshot
            .job_kind
            .unwrap_or(SearchMaintenanceJobKind::LexicalRefresh),
        phase: snapshot.phase.clone(),
        started_at_ms: snapshot.started_at_ms.unwrap_or(0),
        updated_at_ms: snapshot.updated_at_ms.unwrap_or(now_ms),
    }
}

fn maintenance_snapshot_job_id(snapshot: &SearchMaintenanceSnapshot) -> String {
    snapshot
        .job_id
        .as_ref()
        .filter(|job_id| !job_id.is_empty())
        .cloned()
        .unwrap_or_else(|| {
            let mode = snapshot
                .mode
                .map(|mode| mode.as_lock_value())
                .unwrap_or("unknown");
            let owner = snapshot
                .pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "unknown-owner".to_string());
            format!("{mode}-active-lock-{owner}")
        })
}

fn maintenance_snapshot_job_kind(snapshot: &SearchMaintenanceSnapshot) -> SearchMaintenanceJobKind {
    snapshot
        .job_kind
        .unwrap_or(SearchMaintenanceJobKind::LexicalRefresh)
}

fn maintenance_elapsed_ms(snapshot: &SearchMaintenanceSnapshot, now_ms: i64) -> u64 {
    snapshot
        .started_at_ms
        .map(|started_at_ms| u64::try_from(now_ms.saturating_sub(started_at_ms)).unwrap_or(0))
        .unwrap_or(0)
}

fn stale_heartbeat_phase(snapshot: &SearchMaintenanceSnapshot) -> Option<String> {
    snapshot
        .phase
        .clone()
        .or_else(|| Some("stale-heartbeat".to_string()))
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn decide_maintenance_action(data_dir: &Path, now_ms: i64) -> MaintenanceDecision {
    decide_maintenance_action_from_snapshot(&read_search_maintenance_snapshot(data_dir), now_ms)
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn decide_maintenance_action_from_snapshot(
    snapshot: &SearchMaintenanceSnapshot,
    now_ms: i64,
) -> MaintenanceDecision {
    match evaluate_maintenance_coordination_from_snapshot(snapshot, now_ms) {
        MaintenanceCoordinationOutcome::Idle => MaintenanceDecision::Launch,
        MaintenanceCoordinationOutcome::Stale { job_id, .. } => MaintenanceDecision::AttachOrWait {
            job_id,
            job_kind: maintenance_snapshot_job_kind(snapshot),
            phase: stale_heartbeat_phase(snapshot),
            elapsed_ms: maintenance_elapsed_ms(snapshot, now_ms),
        },
        MaintenanceCoordinationOutcome::Active {
            job_id,
            job_kind,
            phase,
            started_at_ms,
            ..
        } => MaintenanceDecision::AttachOrWait {
            job_id,
            job_kind,
            phase,
            elapsed_ms: u64::try_from(now_ms.saturating_sub(started_at_ms)).unwrap_or(0),
        },
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn decide_search_failopen(
    data_dir: &Path,
    now_ms: i64,
    lexical_available: bool,
) -> MaintenanceDecision {
    let snapshot = read_search_maintenance_snapshot(data_dir);
    match evaluate_maintenance_coordination_from_snapshot(&snapshot, now_ms) {
        MaintenanceCoordinationOutcome::Idle => MaintenanceDecision::Launch,
        MaintenanceCoordinationOutcome::Stale { job_id, reason } => {
            if lexical_available {
                MaintenanceDecision::FailOpen {
                    reason: format!(
                        "maintenance job {job_id} has a stale heartbeat ({reason}); lexical index is available, failing open"
                    ),
                }
            } else {
                MaintenanceDecision::AttachOrWait {
                    job_id,
                    job_kind: maintenance_snapshot_job_kind(&snapshot),
                    phase: stale_heartbeat_phase(&snapshot),
                    elapsed_ms: maintenance_elapsed_ms(&snapshot, now_ms),
                }
            }
        }
        MaintenanceCoordinationOutcome::Active {
            job_id,
            job_kind,
            phase,
            started_at_ms,
            ..
        } => {
            if lexical_available {
                MaintenanceDecision::FailOpen {
                    reason: format!(
                        "maintenance job {job_id} is active; lexical index is available, failing open"
                    ),
                }
            } else {
                MaintenanceDecision::AttachOrWait {
                    job_id,
                    job_kind,
                    phase,
                    elapsed_ms: u64::try_from(now_ms.saturating_sub(started_at_ms)).unwrap_or(0),
                }
            }
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct PollResult {
    pub outcome: MaintenanceCoordinationOutcome,
    pub polls: u32,
    pub elapsed: Duration,
    pub timed_out: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn poll_maintenance_until_idle(
    data_dir: &Path,
    timeout: Option<Duration>,
    poll_interval: Option<Duration>,
) -> PollResult {
    let timeout = timeout.unwrap_or(BOUNDED_WAIT_DEFAULT);
    let interval = poll_interval.unwrap_or(POLL_INTERVAL_DEFAULT);
    let start = Instant::now();
    let deadline = start + timeout;
    let mut polls = 0u32;
    loop {
        let now_ms = crate::storage::sqlite::FrankenStorage::now_millis();
        let outcome = evaluate_maintenance_coordination(data_dir, now_ms);
        polls += 1;
        if matches!(outcome, MaintenanceCoordinationOutcome::Idle) {
            return PollResult {
                outcome,
                polls,
                elapsed: start.elapsed(),
                timed_out: false,
            };
        }

        let now = Instant::now();
        if now >= deadline {
            return PollResult {
                outcome,
                polls,
                elapsed: start.elapsed(),
                timed_out: true,
            };
        }
        let remaining = deadline - now;
        std::thread::sleep(interval.min(remaining));
    }
}

// ---------------------------------------------------------------------------
// Rich multi-actor event log, yield/pause signaling, unified view (ibuuh.22)
// ---------------------------------------------------------------------------

#[cfg_attr(not(test), allow(dead_code))]
const MAINTENANCE_EVENTS_FILE: &str = ".maintenance-events.jsonl";
#[cfg_attr(not(test), allow(dead_code))]
const YIELD_SIGNAL_FILE: &str = "maintenance-yield.signal";
#[cfg_attr(not(test), allow(dead_code))]
const MAX_EVENT_LOG_ENTRIES: usize = 500;

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct MaintenanceEvent {
    pub timestamp_ms: i64,
    pub job_id: String,
    pub actor_pid: u32,
    pub kind: MaintenanceEventKind,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum MaintenanceEventKind {
    Started { job_kind: String, phase: String },
    PhaseChanged { from: String, to: String },
    Progress { processed: u64, total: u64 },
    YieldRequested { requester_pid: u32, reason: String },
    Paused { reason: String },
    Resumed,
    Completed { summary: String },
    Failed { error: String },
    Cancelled { reason: String },
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn append_maintenance_event(data_dir: &Path, event: &MaintenanceEvent) -> Result<()> {
    let path = data_dir.join(MAINTENANCE_EVENTS_FILE);
    let line = serde_json::to_string(event).with_context(|| "serializing maintenance event")?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening maintenance event log at {}", path.display()))?;
    use std::io::Write;
    writeln!(file, "{line}")
        .with_context(|| format!("appending to maintenance event log at {}", path.display()))?;
    Ok(())
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn read_maintenance_events(
    data_dir: &Path,
    after_ms: Option<i64>,
    limit: Option<usize>,
) -> Vec<MaintenanceEvent> {
    let path = data_dir.join(MAINTENANCE_EVENTS_FILE);
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let cap = limit.unwrap_or(MAX_EVENT_LOG_ENTRIES);
    contents
        .lines()
        .filter_map(|line| serde_json::from_str::<MaintenanceEvent>(line).ok())
        .filter(|e| after_ms.is_none_or(|threshold| e.timestamp_ms > threshold))
        .rev()
        .take(cap)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn truncate_maintenance_event_log(data_dir: &Path) -> Result<()> {
    let path = data_dir.join(MAINTENANCE_EVENTS_FILE);
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(e).with_context(|| {
                format!("reading event log for truncation at {}", path.display())
            });
        }
    };
    let lines: Vec<&str> = contents.lines().collect();
    if lines.len() <= MAX_EVENT_LOG_ENTRIES {
        return Ok(());
    }
    let keep = &lines[lines.len() - MAX_EVENT_LOG_ENTRIES..];
    let mut output = keep.join("\n");
    output.push('\n');
    std::fs::write(&path, output)
        .with_context(|| format!("truncating event log at {}", path.display()))
}

// ---------------------------------------------------------------------------
// Yield/pause signaling
// ---------------------------------------------------------------------------

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct YieldRequest {
    pub requester_pid: u32,
    pub requested_at_ms: i64,
    pub reason: String,
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn request_yield(data_dir: &Path, reason: &str) -> Result<()> {
    let path = data_dir.join(YIELD_SIGNAL_FILE);
    let now_ms = crate::storage::sqlite::FrankenStorage::now_millis();
    let req = YieldRequest {
        requester_pid: std::process::id(),
        requested_at_ms: now_ms,
        reason: reason.to_string(),
    };
    let payload = serde_json::to_string(&req).with_context(|| "serializing yield request")?;
    std::fs::write(&path, payload)
        .with_context(|| format!("writing yield signal to {}", path.display()))
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn check_yield_requested(data_dir: &Path) -> Option<YieldRequest> {
    let path = data_dir.join(YIELD_SIGNAL_FILE);
    let contents = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str::<YieldRequest>(&contents).ok()
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn clear_yield_signal(data_dir: &Path) -> Result<()> {
    let path = data_dir.join(YIELD_SIGNAL_FILE);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("clearing yield signal at {}", path.display())),
    }
}

// ---------------------------------------------------------------------------
// Unified maintenance view
// ---------------------------------------------------------------------------

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct UnifiedMaintenanceView {
    pub coordination: MaintenanceCoordinationOutcome,
    pub snapshot: SearchMaintenanceSnapshot,
    pub yield_pending: Option<YieldRequest>,
    pub recent_events: Vec<MaintenanceEvent>,
    pub decision: MaintenanceDecision,
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn unified_maintenance_view(
    data_dir: &Path,
    lexical_available: bool,
) -> UnifiedMaintenanceView {
    let now_ms = crate::storage::sqlite::FrankenStorage::now_millis();
    let snapshot = read_search_maintenance_snapshot(data_dir);
    let coordination = evaluate_maintenance_coordination_from_snapshot(&snapshot, now_ms);
    let yield_pending = check_yield_requested(data_dir);
    let recent_events = read_maintenance_events(data_dir, None, Some(20));
    let decision = if lexical_available {
        match &coordination {
            MaintenanceCoordinationOutcome::Active {
                job_id,
                job_kind,
                phase,
                ..
            } => MaintenanceDecision::FailOpen {
                reason: format!(
                    "maintenance job {} ({:?}) is active (phase: {}); lexical available, failing open",
                    job_id,
                    job_kind,
                    phase.as_deref().unwrap_or("unknown")
                ),
            },
            MaintenanceCoordinationOutcome::Stale { job_id, reason } => {
                MaintenanceDecision::FailOpen {
                    reason: format!(
                        "maintenance job {job_id} has a stale heartbeat ({reason}); lexical available, failing open"
                    ),
                }
            }
            _ => decide_maintenance_action_from_snapshot(&snapshot, now_ms),
        }
    } else {
        decide_maintenance_action_from_snapshot(&snapshot, now_ms)
    };

    UnifiedMaintenanceView {
        coordination,
        snapshot,
        yield_pending,
        recent_events,
        decision,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    fn active_lock_fixture_pid(_non_windows_pid: u32) -> u32 {
        std::process::id()
    }

    #[cfg(not(windows))]
    fn active_lock_fixture_pid(non_windows_pid: u32) -> u32 {
        non_windows_pid
    }

    fn write_locked_metadata(
        lock_path: &Path,
        owner: &mut std::fs::File,
        contents: &str,
    ) -> std::io::Result<()> {
        use std::io::{Seek, SeekFrom, Write};

        owner.set_len(0)?;
        owner.seek(SeekFrom::Start(0))?;
        owner.write_all(contents.as_bytes())?;
        owner.flush()?;
        owner.sync_all()?;
        write_index_run_lock_metadata_sidecar(lock_path, contents)
            .map_err(std::io::Error::other)?;
        Ok(())
    }

    #[test]
    fn maintenance_mode_round_trips_lock_values() {
        for mode in [
            SearchMaintenanceMode::Index,
            SearchMaintenanceMode::WatchStartup,
            SearchMaintenanceMode::Watch,
            SearchMaintenanceMode::WatchOnce,
        ] {
            assert_eq!(
                SearchMaintenanceMode::parse_lock_value(mode.as_lock_value()),
                Some(mode)
            );
        }
    }

    #[test]
    fn maintenance_job_kind_round_trips_lock_values() {
        for kind in [
            SearchMaintenanceJobKind::LexicalRefresh,
            SearchMaintenanceJobKind::SemanticAcquire,
        ] {
            assert_eq!(
                SearchMaintenanceJobKind::parse_lock_value(kind.as_lock_value()),
                Some(kind)
            );
        }
    }

    #[test]
    fn stale_lock_metadata_from_dead_owner_is_reaped_on_read() {
        // Regression for issue #176: the TUI used to see a permanent
        // `orphaned: true` state when the index-run.lock file contained
        // metadata from a crashed process, because nothing in the read
        // path cleaned it up. That produced a tight CPU-bound poll loop
        // on startup. The read path now reaps stale metadata atomically
        // while holding the exclusive flock.
        let temp = tempfile::tempdir().expect("tempdir");
        let lock_path = temp.path().join("index-run.lock");
        // The reap path does not probe the recorded pid — POSIX flock
        // acquisition is the signal — so the concrete pid value in the
        // fixture is irrelevant. We still record one so the parser
        // runs through its full happy path.
        std::fs::write(
            &lock_path,
            concat!(
                "pid=4242\n",
                "started_at_ms=1733000111000\n",
                "updated_at_ms=1733000112000\n",
                "db_path=/tmp/cass/agent_search.db\n",
                "mode=index\n",
                "job_id=lexical-refresh-1733000111000-4242\n",
                "job_kind=lexical_refresh\n",
                "phase=rebuilding\n"
            ),
        )
        .expect("write lock metadata");

        let snapshot = read_search_maintenance_snapshot(temp.path());
        assert!(!snapshot.active, "no owner, must not be reported active");
        assert!(
            !snapshot.orphaned,
            "stale metadata must be reaped, not reported as orphaned"
        );
        assert!(snapshot.pid.is_none(), "pid must be cleared after reap");
        assert!(
            snapshot.job_id.is_none(),
            "job_id must be cleared after reap"
        );
        assert!(snapshot.phase.is_none(), "phase must be cleared after reap");

        // File must still exist (to preserve permissions and avoid
        // creating/recreating races) but be empty.
        let post = std::fs::metadata(&lock_path).expect("lock file still present");
        assert_eq!(post.len(), 0, "stale metadata must be truncated in place");

        // Second read also returns a clean default snapshot.
        let snapshot2 = read_search_maintenance_snapshot(temp.path());
        assert!(!snapshot2.active);
        assert!(!snapshot2.orphaned);
    }

    #[test]
    fn live_owner_metadata_is_preserved_when_flock_is_held() -> std::io::Result<()> {
        // When the lock is actually held by a live owner, the snapshot
        // must report the owner faithfully and must NOT reap the file.
        use fs2::FileExt;
        let temp = tempfile::tempdir()?;
        let lock_path = temp.path().join("index-run.lock");
        let owner_pid = active_lock_fixture_pid(4242);
        let mut owner = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(&lock_path)?;
        owner.try_lock_exclusive()?;
        // Write metadata while holding the lock, matching acquire_index_run_lock's order.
        write_locked_metadata(
            &lock_path,
            &mut owner,
            format!(
                concat!(
                    "pid={}\n",
                    "started_at_ms=1733000111000\n",
                    "updated_at_ms=1733000112000\n",
                    "db_path=/tmp/cass/agent_search.db\n",
                    "mode=index\n",
                    "job_id=lexical-refresh-1733000111000-4242\n",
                    "job_kind=lexical_refresh\n",
                    "phase=rebuilding\n"
                ),
                owner_pid
            )
            .as_str(),
        )?;

        let snapshot = read_search_maintenance_snapshot(temp.path());
        assert!(snapshot.active, "live owner must be reported active");
        assert!(!snapshot.orphaned);
        assert_eq!(snapshot.pid, Some(owner_pid));
        assert_eq!(
            snapshot.job_id.as_deref(),
            Some("lexical-refresh-1733000111000-4242")
        );
        assert_eq!(
            snapshot.job_kind,
            Some(SearchMaintenanceJobKind::LexicalRefresh)
        );
        assert_eq!(snapshot.phase.as_deref(), Some("rebuilding"));
        assert_eq!(snapshot.updated_at_ms, Some(1_733_000_112_000));

        // Metadata must still be present — reaping must NOT have happened.
        let post = std::fs::metadata(&lock_path)?;
        assert!(post.len() > 0, "live-owner metadata must not be truncated");

        FileExt::unlock(&owner)?;
        Ok(())
    }

    #[test]
    fn lexical_storage_fingerprint_matching_handles_jitter_and_size_drift() {
        let cases = [
            (
                "small mtime settle jitter",
                "323584:1776310228000:329632:1776310227824",
                "323584:1776310227832:329632:1776310227824",
                true,
            ),
            (
                "wal size drift",
                "323584:1776310228000:329632:1776310227824",
                "323584:1776310227832:400000:1776310227824",
                false,
            ),
        ];

        for (label, current, saved, expected) in cases {
            assert_eq!(
                lexical_storage_fingerprints_match(current, saved),
                expected,
                "{label}"
            );
        }
    }

    #[test]
    fn lexical_state_marks_fingerprint_mismatch_stale() {
        let temp = tempfile::tempdir().expect("tempdir");
        let index_path = temp.path().join("index").join("v4");
        std::fs::create_dir_all(&index_path).expect("create index dir");
        // Simulate an existing tantivy index (meta.json present) so the
        // "missing" branch in lexical_state_from_observations doesn't short
        // circuit before the fingerprint check we want to exercise.
        std::fs::write(index_path.join("meta.json"), b"{}").expect("write meta.json");
        let db_path = temp.path().join("agent_search.db");
        std::fs::write(&db_path, b"db").expect("write db file");

        let checkpoint = LexicalRebuildCheckpoint {
            db_path: db_path.display().to_string(),
            total_conversations: 10,
            storage_fingerprint: "before".to_string(),
            committed_offset: 10,
            committed_conversation_id: Some(10),
            processed_conversations: 10,
            indexed_docs: 100,
            schema_hash: SCHEMA_HASH.to_string(),
            page_size: LEXICAL_REBUILD_PAGE_SIZE_PUBLIC,
            completed: true,
            updated_at_ms: 1_733_000_000_000,
        };

        let state = lexical_state_from_observations(LexicalObservationInput {
            index_path: &index_path,
            db_path: &db_path,
            stale_threshold: 60,
            last_indexed_at_ms: Some(1_733_000_000_000),
            now_ms: 1_733_000_001_000,
            maintenance: SearchMaintenanceSnapshot::default(),
            checkpoint: Some(&checkpoint),
            current_db_fingerprint: Some("after"),
        });

        assert_eq!(state.status, "stale");
        assert_eq!(
            state.fingerprint.matches_current_db_fingerprint,
            Some(false)
        );
        assert!(
            state
                .status_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("fingerprint"))
        );
        assert_eq!(state.pending_sessions, 0);
        assert_eq!(state.processed_conversations, None);
        assert_eq!(state.total_conversations, None);
        assert_eq!(state.indexed_docs, None);
    }

    #[test]
    fn lexical_state_marks_checkpoint_db_mismatch_stale_without_fingerprint_probe() {
        let temp = tempfile::tempdir().expect("tempdir");
        let index_path = temp.path().join("index").join("v4");
        std::fs::create_dir_all(&index_path).expect("create index dir");
        std::fs::write(index_path.join("meta.json"), b"{}").expect("write meta.json");
        let db_path = temp.path().join("agent_search.db");
        let other_db_path = temp.path().join("other_agent_search.db");
        std::fs::write(&db_path, b"db").expect("write db file");
        std::fs::write(&other_db_path, b"other db").expect("write other db file");

        let checkpoint = LexicalRebuildCheckpoint {
            db_path: other_db_path.display().to_string(),
            total_conversations: 10,
            storage_fingerprint: "old-db-fingerprint".to_string(),
            committed_offset: 10,
            committed_conversation_id: Some(10),
            processed_conversations: 10,
            indexed_docs: 100,
            schema_hash: SCHEMA_HASH.to_string(),
            page_size: LEXICAL_REBUILD_PAGE_SIZE_PUBLIC,
            completed: true,
            updated_at_ms: 1_733_000_000_000,
        };

        let state = lexical_state_from_observations(LexicalObservationInput {
            index_path: &index_path,
            db_path: &db_path,
            stale_threshold: 60,
            last_indexed_at_ms: Some(1_733_000_000_000),
            now_ms: 1_733_000_001_000,
            maintenance: SearchMaintenanceSnapshot::default(),
            checkpoint: Some(&checkpoint),
            current_db_fingerprint: None,
        });

        assert_eq!(state.status, "stale");
        assert!(state.stale);
        assert!(!state.fresh);
        assert_eq!(state.checkpoint.db_matches, Some(false));
        assert_eq!(state.fingerprint.matches_current_db_fingerprint, None);
        assert_eq!(state.pending_sessions, 0);
        assert_eq!(state.processed_conversations, None);
        assert_eq!(state.total_conversations, None);
        assert_eq!(state.indexed_docs, None);
        assert!(
            state
                .status_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("different database"))
        );
    }

    #[test]
    fn lexical_state_missing_index_is_not_marked_stale_until_initialized() {
        let temp = tempfile::tempdir().expect("tempdir");
        let index_path = temp.path().join("index").join("v4");
        std::fs::create_dir_all(&index_path).expect("create index dir");
        let db_path = temp.path().join("agent_search.db");
        std::fs::write(&db_path, b"db").expect("write db file");

        let state = lexical_state_from_observations(LexicalObservationInput {
            index_path: &index_path,
            db_path: &db_path,
            stale_threshold: 60,
            last_indexed_at_ms: None,
            now_ms: 1_733_000_001_000,
            maintenance: SearchMaintenanceSnapshot::default(),
            checkpoint: None,
            current_db_fingerprint: None,
        });

        assert_eq!(state.status, "missing");
        assert!(!state.exists);
        assert!(!state.stale);
        assert!(!state.fresh);
        assert_eq!(
            state.status_reason.as_deref(),
            Some("lexical Tantivy metadata missing")
        );
    }

    #[test]
    fn lexical_state_keeps_progress_visible_during_active_rebuild_despite_fingerprint_drift() {
        let temp = tempfile::tempdir().expect("tempdir");
        let index_path = temp.path().join("index").join("v4");
        std::fs::create_dir_all(&index_path).expect("create index dir");
        std::fs::write(index_path.join("meta.json"), b"{}").expect("write meta.json");
        let db_path = temp.path().join("agent_search.db");
        std::fs::write(&db_path, b"db").expect("write db file");

        let checkpoint = LexicalRebuildCheckpoint {
            db_path: db_path.display().to_string(),
            total_conversations: 10,
            storage_fingerprint: "before".to_string(),
            committed_offset: 4,
            committed_conversation_id: Some(4),
            processed_conversations: 4,
            indexed_docs: 20,
            schema_hash: SCHEMA_HASH.to_string(),
            page_size: 200,
            completed: false,
            updated_at_ms: 1_733_000_123_000,
        };

        let state = lexical_state_from_observations(LexicalObservationInput {
            index_path: &index_path,
            db_path: &db_path,
            stale_threshold: 60,
            last_indexed_at_ms: Some(1_733_000_000_000),
            now_ms: 1_733_000_001_000,
            maintenance: SearchMaintenanceSnapshot {
                active: true,
                pid: Some(std::process::id()),
                started_at_ms: Some(1_733_000_111_000),
                db_path: Some(db_path.clone()),
                mode: Some(SearchMaintenanceMode::Index),
                job_id: None,
                job_kind: None,
                phase: None,
                updated_at_ms: None,
                last_progress_at_ms: None,
                orphaned: false,
            },
            checkpoint: Some(&checkpoint),
            current_db_fingerprint: Some("after"),
        });

        assert_eq!(state.status, "building");
        assert!(!state.stale);
        assert!(!state.fresh);
        assert_eq!(state.pending_sessions, 6);
        assert_eq!(state.processed_conversations, Some(4));
        assert_eq!(state.total_conversations, Some(10));
        assert_eq!(state.indexed_docs, Some(20));
        assert_eq!(state.checkpoint.page_size_matches, Some(false));
        assert_eq!(state.checkpoint.page_size_compatible, Some(true));
        assert_eq!(
            state.status_reason.as_deref(),
            Some("lexical rebuild is in progress")
        );
    }

    #[test]
    fn lexical_state_hides_progress_for_incompatible_page_size_checkpoint() {
        let temp = tempfile::tempdir().expect("tempdir");
        let index_path = temp.path().join("index").join("v4");
        std::fs::create_dir_all(&index_path).expect("create index dir");
        std::fs::write(index_path.join("meta.json"), b"{}").expect("write meta.json");
        let db_path = temp.path().join("agent_search.db");
        std::fs::write(&db_path, b"db").expect("write db file");

        let checkpoint = LexicalRebuildCheckpoint {
            db_path: db_path.display().to_string(),
            total_conversations: 10,
            storage_fingerprint: "before".to_string(),
            committed_offset: 4,
            committed_conversation_id: Some(4),
            processed_conversations: 4,
            indexed_docs: 20,
            schema_hash: SCHEMA_HASH.to_string(),
            page_size: 13,
            completed: false,
            updated_at_ms: 1_733_000_123_000,
        };

        let state = lexical_state_from_observations(LexicalObservationInput {
            index_path: &index_path,
            db_path: &db_path,
            stale_threshold: 60,
            last_indexed_at_ms: Some(1_733_000_000_000),
            now_ms: 1_733_000_001_000,
            maintenance: SearchMaintenanceSnapshot::default(),
            checkpoint: Some(&checkpoint),
            current_db_fingerprint: Some("before"),
        });

        assert_eq!(state.status, "stale");
        assert!(state.stale);
        assert_eq!(state.pending_sessions, 0);
        assert_eq!(state.processed_conversations, None);
        assert_eq!(state.total_conversations, None);
        assert_eq!(state.indexed_docs, None);
        assert_eq!(state.checkpoint.page_size_matches, Some(false));
        assert_eq!(state.checkpoint.page_size_compatible, Some(false));
        assert!(
            state
                .status_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("contract"))
        );
    }

    #[test]
    fn lexical_state_prefers_newer_maintenance_heartbeat_over_stale_checkpoint_timestamp() {
        let temp = tempfile::tempdir().expect("tempdir");
        let index_path = temp.path().join("index").join("v4");
        std::fs::create_dir_all(&index_path).expect("create index dir");
        let db_path = temp.path().join("agent_search.db");
        std::fs::write(&db_path, b"db").expect("write db file");

        let checkpoint = LexicalRebuildCheckpoint {
            db_path: db_path.display().to_string(),
            total_conversations: 10,
            storage_fingerprint: "before".to_string(),
            committed_offset: 4,
            committed_conversation_id: Some(4),
            processed_conversations: 4,
            indexed_docs: 20,
            schema_hash: SCHEMA_HASH.to_string(),
            page_size: LEXICAL_REBUILD_PAGE_SIZE_PUBLIC,
            completed: false,
            updated_at_ms: 1_733_000_123_000,
        };

        let state = lexical_state_from_observations(LexicalObservationInput {
            index_path: &index_path,
            db_path: &db_path,
            stale_threshold: 60,
            last_indexed_at_ms: Some(1_733_000_000_000),
            now_ms: 1_733_000_001_000,
            maintenance: SearchMaintenanceSnapshot {
                active: true,
                pid: Some(std::process::id()),
                started_at_ms: Some(1_733_000_111_000),
                db_path: Some(db_path.clone()),
                mode: Some(SearchMaintenanceMode::Index),
                job_id: None,
                job_kind: None,
                phase: None,
                updated_at_ms: Some(1_733_000_456_000),
                last_progress_at_ms: None,
                orphaned: false,
            },
            checkpoint: Some(&checkpoint),
            current_db_fingerprint: Some("after"),
        });

        assert_eq!(state.status, "building");
        assert_eq!(state.activity_at_ms, Some(1_733_000_456_000));
    }

    #[test]
    fn lexical_state_ignores_rebuild_lock_for_different_database() {
        let temp = tempfile::tempdir().expect("tempdir");
        let index_path = temp.path().join("index").join("v4");
        std::fs::create_dir_all(&index_path).expect("create index dir");
        std::fs::write(index_path.join("meta.json"), b"{}").expect("write meta.json");
        let db_path = temp.path().join("agent_search.db");
        std::fs::write(&db_path, b"db").expect("write db file");
        let other_db_path = temp.path().join("other.db");
        std::fs::write(&other_db_path, b"other").expect("write other db file");

        let checkpoint = LexicalRebuildCheckpoint {
            db_path: db_path.display().to_string(),
            total_conversations: 10,
            storage_fingerprint: "before".to_string(),
            committed_offset: 4,
            committed_conversation_id: Some(4),
            processed_conversations: 4,
            indexed_docs: 20,
            schema_hash: SCHEMA_HASH.to_string(),
            page_size: LEXICAL_REBUILD_PAGE_SIZE_PUBLIC,
            completed: false,
            updated_at_ms: 1_733_000_123_000,
        };

        let state = lexical_state_from_observations(LexicalObservationInput {
            index_path: &index_path,
            db_path: &db_path,
            stale_threshold: 60,
            last_indexed_at_ms: Some(1_733_000_000_000),
            now_ms: 1_733_000_001_000,
            maintenance: SearchMaintenanceSnapshot {
                active: true,
                pid: Some(std::process::id()),
                started_at_ms: Some(1_733_000_111_000),
                db_path: Some(other_db_path),
                mode: Some(SearchMaintenanceMode::Index),
                job_id: None,
                job_kind: None,
                phase: None,
                updated_at_ms: None,
                last_progress_at_ms: None,
                orphaned: false,
            },
            checkpoint: Some(&checkpoint),
            current_db_fingerprint: Some("after"),
        });

        assert_eq!(state.status, "stale");
        assert!(state.stale);
        assert!(!state.fresh);
        assert!(!state.rebuilding);
        assert!(!state.watch_active);
        assert_eq!(state.activity_at_ms, None);
        assert_eq!(state.pending_sessions, 0);
        assert_eq!(state.processed_conversations, None);
        assert_eq!(state.total_conversations, None);
        assert_eq!(state.indexed_docs, None);
        assert!(
            state
                .status_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("fingerprint"))
        );
    }

    #[test]
    fn lexical_state_ignores_watch_lock_for_different_database() {
        let temp = tempfile::tempdir().expect("tempdir");
        let index_path = temp.path().join("index").join("v4");
        std::fs::create_dir_all(&index_path).expect("create index dir");
        std::fs::write(index_path.join("meta.json"), b"{}").expect("write meta.json");
        let db_path = temp.path().join("agent_search.db");
        std::fs::write(&db_path, b"db").expect("write db file");
        let other_db_path = temp.path().join("other.db");
        std::fs::write(&other_db_path, b"other").expect("write other db file");

        let state = lexical_state_from_observations(LexicalObservationInput {
            index_path: &index_path,
            db_path: &db_path,
            stale_threshold: 60,
            last_indexed_at_ms: Some(1_733_000_000_000),
            now_ms: 1_733_000_020_000,
            maintenance: SearchMaintenanceSnapshot {
                active: true,
                pid: Some(std::process::id()),
                started_at_ms: Some(1_733_000_111_000),
                db_path: Some(other_db_path),
                mode: Some(SearchMaintenanceMode::Watch),
                job_id: None,
                job_kind: None,
                phase: None,
                updated_at_ms: None,
                last_progress_at_ms: None,
                orphaned: false,
            },
            checkpoint: None,
            current_db_fingerprint: None,
        });

        assert_eq!(state.status, "ready");
        assert!(state.fresh);
        assert!(!state.stale);
        assert!(!state.rebuilding);
        assert!(!state.watch_active);
        assert_eq!(state.activity_at_ms, None);
    }

    // ---- Forward-progress liveness / stall detection (issue #258) ----

    /// `last_progress_at_ms` is older than the default 120 s stall
    /// threshold; the heartbeat `updated_at_ms` is fresh (the heartbeat
    /// thread kept refreshing it independently). Status must flip to
    /// `stalled`, NOT remain `building` — this is the regression #258
    /// guards against.
    #[test]
    fn lexical_state_reports_stalled_when_progress_is_stale_despite_fresh_heartbeat() {
        let temp = tempfile::tempdir().expect("tempdir");
        let index_path = temp.path().join("index").join("v4");
        std::fs::create_dir_all(&index_path).expect("create index dir");
        std::fs::write(index_path.join("meta.json"), b"{}").expect("write meta.json");
        let db_path = temp.path().join("agent_search.db");
        std::fs::write(&db_path, b"db").expect("write db file");

        // now = 1_733_000_300 s (= 1_733_000_300_000 ms).
        // heartbeat updated 500 ms ago: fresh.
        // forward progress posted 300 s ago: well past the 120 s default stall threshold.
        // F4 (cass tech debt): the input now carries full-precision
        // `now_ms` end-to-end. Tests that previously needed
        // `now_secs as i64 * 1000` no longer have to round-trip.
        let now_ms: i64 = 1_733_000_300_000;

        let state = lexical_state_from_observations(LexicalObservationInput {
            index_path: &index_path,
            db_path: &db_path,
            stale_threshold: 60,
            last_indexed_at_ms: Some(1_733_000_000_000),
            now_ms,
            maintenance: SearchMaintenanceSnapshot {
                active: true,
                pid: Some(std::process::id()),
                started_at_ms: Some(now_ms - 600_000),
                db_path: Some(db_path.clone()),
                mode: Some(SearchMaintenanceMode::WatchStartup),
                job_id: Some("lexical_refresh-1-1".to_string()),
                job_kind: Some(SearchMaintenanceJobKind::LexicalRefresh),
                phase: Some("watch_startup".to_string()),
                updated_at_ms: Some(now_ms - 500),
                last_progress_at_ms: Some(now_ms - 300_000),
                orphaned: false,
            },
            checkpoint: None,
            current_db_fingerprint: None,
        });

        assert!(state.rebuilding, "active rebuild lock must still register");
        assert!(
            state.stalled,
            "stale forward-progress timestamp must flip stalled=true",
        );
        assert_eq!(state.status, "stalled");
        assert_eq!(
            state.last_progress_at_ms,
            Some(now_ms - 300_000),
            "last_progress_at_ms must be surfaced to status callers",
        );
        let age = state
            .last_progress_age_ms
            .expect("last_progress_age_ms must be computed");
        assert!(
            (299_900..=300_100).contains(&age),
            "computed last_progress_age_ms ({age}ms) should equal now - last_progress_at_ms (300_000ms)",
        );
        let reason = state
            .status_reason
            .as_deref()
            .expect("stalled state should populate status_reason");
        assert!(
            reason.contains("forward progress")
                && (reason.contains("#258") || reason.contains("issue #258")),
            "status_reason should mention forward progress and reference #258 ({reason})",
        );
    }

    /// Heartbeat is fresh AND forward-progress is fresh: no stall.
    /// Status remains `building`. Ensures the new gate does not regress
    /// the happy path.
    #[test]
    fn lexical_state_stays_building_when_progress_is_recent() {
        let temp = tempfile::tempdir().expect("tempdir");
        let index_path = temp.path().join("index").join("v4");
        std::fs::create_dir_all(&index_path).expect("create index dir");
        std::fs::write(index_path.join("meta.json"), b"{}").expect("write meta.json");
        let db_path = temp.path().join("agent_search.db");
        std::fs::write(&db_path, b"db").expect("write db file");

        // F4 (cass tech debt): the input now carries full-precision
        // `now_ms` end-to-end. Tests that previously needed
        // `now_secs as i64 * 1000` no longer have to round-trip.
        let now_ms: i64 = 1_733_000_300_000;

        let state = lexical_state_from_observations(LexicalObservationInput {
            index_path: &index_path,
            db_path: &db_path,
            stale_threshold: 60,
            last_indexed_at_ms: Some(1_733_000_000_000),
            now_ms,
            maintenance: SearchMaintenanceSnapshot {
                active: true,
                pid: Some(std::process::id()),
                started_at_ms: Some(now_ms - 30_000),
                db_path: Some(db_path.clone()),
                mode: Some(SearchMaintenanceMode::Index),
                job_id: Some("lexical_refresh-1-1".to_string()),
                job_kind: Some(SearchMaintenanceJobKind::LexicalRefresh),
                phase: Some("scanning".to_string()),
                updated_at_ms: Some(now_ms - 500),
                last_progress_at_ms: Some(now_ms - 1_000),
                orphaned: false,
            },
            checkpoint: None,
            current_db_fingerprint: None,
        });

        assert!(state.rebuilding);
        assert!(!state.stalled, "fresh progress must not flip stalled");
        assert_eq!(state.status, "building");
    }

    /// Legacy lock files (older cass that didn't write
    /// `last_progress_at_ms`) must not be misreported as stalled. The
    /// stall check only fires when an explicit `last_progress_at_ms`
    /// is present; absent that, we fall back to the previous behavior.
    #[test]
    fn lexical_state_does_not_stall_when_legacy_lock_omits_progress_field() {
        let temp = tempfile::tempdir().expect("tempdir");
        let index_path = temp.path().join("index").join("v4");
        std::fs::create_dir_all(&index_path).expect("create index dir");
        std::fs::write(index_path.join("meta.json"), b"{}").expect("write meta.json");
        let db_path = temp.path().join("agent_search.db");
        std::fs::write(&db_path, b"db").expect("write db file");

        // F4 (cass tech debt): the input now carries full-precision
        // `now_ms` end-to-end. Tests that previously needed
        // `now_secs as i64 * 1000` no longer have to round-trip.
        let now_ms: i64 = 1_733_000_300_000;

        let state = lexical_state_from_observations(LexicalObservationInput {
            index_path: &index_path,
            db_path: &db_path,
            stale_threshold: 60,
            last_indexed_at_ms: Some(1_733_000_000_000),
            now_ms,
            maintenance: SearchMaintenanceSnapshot {
                active: true,
                pid: Some(std::process::id()),
                started_at_ms: Some(now_ms - 30_000),
                db_path: Some(db_path.clone()),
                mode: Some(SearchMaintenanceMode::Index),
                job_id: None,
                job_kind: None,
                phase: None,
                updated_at_ms: Some(now_ms - 500),
                last_progress_at_ms: None,
                orphaned: false,
            },
            checkpoint: None,
            current_db_fingerprint: None,
        });

        assert!(state.rebuilding);
        assert!(
            !state.stalled,
            "legacy lock without last_progress_at_ms must NOT be misreported as stalled",
        );
        assert_eq!(state.status, "building");
        assert!(state.last_progress_age_ms.is_none());
    }

    /// F4 (cass tech debt): the stall-age comparison must operate at
    /// full millisecond precision. Pre-fix, `now_ms` was derived from
    /// `now_secs * 1000` inside `lexical_state_from_observations`, which
    /// quantised the comparison to second resolution and made a
    /// 119_900 ms-old progress timestamp indistinguishable from a 119
    /// 000 ms-old one. This test pins a 119_500 ms age so that the only
    /// way it surfaces correctly as a `last_progress_age_ms` close to
    /// 119_500 (and NOT 119_000 or 120_000) is full-ms plumbing.
    #[test]
    fn lexical_state_progress_age_is_ms_precision_not_seconds_quantised() {
        let temp = tempfile::tempdir().expect("tempdir");
        let index_path = temp.path().join("index").join("v4");
        std::fs::create_dir_all(&index_path).expect("create index dir");
        std::fs::write(index_path.join("meta.json"), b"{}").expect("write meta.json");
        let db_path = temp.path().join("agent_search.db");
        std::fs::write(&db_path, b"db").expect("write db file");

        // `now_ms` deliberately lands at .700 of a second so any
        // second-quantisation upstream would shave 700 ms off the diff.
        let now_ms: i64 = 1_733_000_300_700;
        let last_progress_at_ms = now_ms - 119_500;

        let state = lexical_state_from_observations(LexicalObservationInput {
            index_path: &index_path,
            db_path: &db_path,
            stale_threshold: 60,
            last_indexed_at_ms: Some(1_733_000_000_000),
            now_ms,
            maintenance: SearchMaintenanceSnapshot {
                active: true,
                pid: Some(std::process::id()),
                started_at_ms: Some(now_ms - 600_000),
                db_path: Some(db_path.clone()),
                mode: Some(SearchMaintenanceMode::WatchStartup),
                job_id: Some("lexical_refresh-1-1".to_string()),
                job_kind: Some(SearchMaintenanceJobKind::LexicalRefresh),
                phase: Some("watch_startup".to_string()),
                updated_at_ms: Some(now_ms - 500),
                last_progress_at_ms: Some(last_progress_at_ms),
                orphaned: false,
            },
            checkpoint: None,
            current_db_fingerprint: None,
        });

        let age = state
            .last_progress_age_ms
            .expect("forward-progress age must be computed");
        assert_eq!(
            age, 119_500,
            "ms-precision plumbing must surface the exact diff (no second-quantisation)"
        );
        // 119_500 ms is still under the default 120 s stall threshold,
        // so we should be `building`, not `stalled`. Pre-F4, a
        // second-quantised clock could either floor the diff to 119_000
        // (still building, OK) OR — on different `.fff` ms suffixes —
        // round it to 120_000 (false-positive stall). Pinning the
        // expected status here protects both edges.
        assert!(
            !state.stalled,
            "119.5 s lag must remain `building`, not flip to `stalled`",
        );
        assert_eq!(state.status, "building");
    }

    /// Coordination outcome layer must also degrade to `Stale` when
    /// forward progress is stuck — search-side single-flight callers
    /// then route around the wedged worker instead of attaching to it.
    #[test]
    fn coordination_reports_stale_when_forward_progress_is_stuck() {
        let now_ms: i64 = 1_733_000_300_000;
        let snapshot = SearchMaintenanceSnapshot {
            active: true,
            pid: Some(12345),
            started_at_ms: Some(now_ms - 600_000),
            db_path: Some(PathBuf::from("/tmp/cass/agent_search.db")),
            mode: Some(SearchMaintenanceMode::WatchStartup),
            job_id: Some("lexical_refresh-1-12345".to_string()),
            job_kind: Some(SearchMaintenanceJobKind::LexicalRefresh),
            phase: Some("watch_startup".to_string()),
            updated_at_ms: Some(now_ms - 500),
            last_progress_at_ms: Some(now_ms - 300_000),
            orphaned: false,
        };

        let outcome = evaluate_maintenance_coordination_from_snapshot(&snapshot, now_ms);
        match outcome {
            MaintenanceCoordinationOutcome::Stale { ref reason, .. } => {
                assert!(
                    reason.contains("forward progress")
                        && (reason.contains("#258") || reason.contains("issue #258")),
                    "stalled coordination reason should mention forward progress and #258: {reason}",
                );
            }
            other => assert!(
                matches!(other, MaintenanceCoordinationOutcome::Stale { .. }),
                "stalled forward-progress snapshot must coordinate as Stale, got {other:?}",
            ),
        }
    }

    #[test]
    fn inspect_search_assets_preserves_semantic_database_unavailable_signal() {
        let temp = tempfile::tempdir().expect("tempdir");
        let index_path = temp.path().join("index").join("v4");
        std::fs::create_dir_all(&index_path).expect("create index dir");
        std::fs::write(index_path.join("meta.json"), b"{}").expect("write meta.json");

        let db_path = temp.path().join("agent_search.db");
        std::fs::create_dir_all(&db_path).expect("create unopenable db path");

        let vector_path = vector_index_path(temp.path(), HashEmbedder::default().id());
        std::fs::create_dir_all(vector_path.parent().expect("vector parent"))
            .expect("create vector dir");
        std::fs::write(&vector_path, b"index").expect("write vector index");

        let snapshot = inspect_search_assets(InspectSearchAssetsInput {
            data_dir: temp.path(),
            db_path: &db_path,
            stale_threshold: 60,
            last_indexed_at_ms: Some(1_733_000_000_000),
            now_ms: 1_733_000_001_000,
            maintenance: SearchMaintenanceSnapshot::default(),
            semantic_preference: SemanticPreference::HashFallback,
            db_available: false,
            compute_lexical_fingerprint: false,
            inspect_semantic: true,
        })
        .expect("asset inspection should not fail when db availability is already known");

        assert_ne!(snapshot.lexical.status, "error");
        assert_eq!(snapshot.semantic.status, "error");
        assert_eq!(snapshot.semantic.availability, "database_unavailable");
        assert_eq!(snapshot.semantic.fallback_mode, Some("lexical"));
        assert!(snapshot.semantic.summary.contains("db unavailable"));
    }

    #[test]
    fn inspect_search_assets_can_skip_semantic_db_open_for_fast_paths() {
        let temp = tempfile::tempdir().expect("tempdir");
        let index_path = temp.path().join("index").join("v4");
        std::fs::create_dir_all(&index_path).expect("create index dir");
        std::fs::write(index_path.join("meta.json"), b"{}").expect("write meta.json");

        let db_path = temp.path().join("agent_search.db");
        std::fs::create_dir_all(&db_path).expect("create unopenable db path");

        let vector_path = vector_index_path(temp.path(), HashEmbedder::default().id());
        std::fs::create_dir_all(vector_path.parent().expect("vector parent"))
            .expect("create vector dir");
        std::fs::write(&vector_path, b"index").expect("write vector index");

        let snapshot = inspect_search_assets(InspectSearchAssetsInput {
            data_dir: temp.path(),
            db_path: &db_path,
            stale_threshold: 60,
            last_indexed_at_ms: Some(1_733_000_000_000),
            now_ms: 1_733_000_001_000,
            maintenance: SearchMaintenanceSnapshot::default(),
            semantic_preference: SemanticPreference::HashFallback,
            db_available: false,
            compute_lexical_fingerprint: false,
            inspect_semantic: false,
        })
        .expect("asset inspection should not open semantic DB when semantic inspection is skipped");

        assert_ne!(snapshot.lexical.status, "error");
        assert_eq!(snapshot.semantic.status, "not_inspected");
        assert_eq!(snapshot.semantic.availability, "not_inspected");
        assert_eq!(snapshot.semantic.fallback_mode, Some("lexical"));
    }

    #[test]
    fn inspect_search_assets_trusts_db_probe_for_semantic_metadata_probe() {
        let temp = tempfile::tempdir().expect("tempdir");
        let index_path = temp.path().join("index").join("v4");
        std::fs::create_dir_all(&index_path).expect("create index dir");
        std::fs::write(index_path.join("meta.json"), b"{}").expect("write meta.json");

        let db_path = temp.path().join("agent_search.db");
        std::fs::create_dir_all(&db_path).expect("create unopenable db path");

        let vector_path = vector_index_path(temp.path(), HashEmbedder::default().id());
        std::fs::create_dir_all(vector_path.parent().expect("vector parent"))
            .expect("create vector dir");
        std::fs::write(&vector_path, b"index").expect("write vector index");

        let snapshot = inspect_search_assets(InspectSearchAssetsInput {
            data_dir: temp.path(),
            db_path: &db_path,
            stale_threshold: 60,
            last_indexed_at_ms: Some(1_733_000_000_000),
            now_ms: 1_733_000_001_000,
            maintenance: SearchMaintenanceSnapshot::default(),
            semantic_preference: SemanticPreference::HashFallback,
            db_available: true,
            compute_lexical_fingerprint: false,
            inspect_semantic: true,
        })
        .expect("semantic metadata probe should trust the existing DB availability signal");

        assert_eq!(snapshot.semantic.status, "hash_fallback");
        assert_eq!(snapshot.semantic.availability, "hash_fallback");
        assert!(snapshot.semantic.can_search);
    }

    #[test]
    fn semantic_state_reports_hash_fallback_as_searchable() {
        let state = semantic_state_from_availability(
            Path::new("/tmp/cass"),
            &SemanticAvailability::HashFallback,
            SemanticPreference::HashFallback,
            None,
        );

        assert_eq!(state.status, "hash_fallback");
        assert_eq!(state.availability, "hash_fallback");
        assert!(state.available);
        assert!(state.can_search);
        assert_eq!(state.fallback_mode, None);
    }

    #[test]
    fn semantic_preference_surface_preserves_backend_and_model_dir_projection() {
        let data_dir = Path::new("/tmp/cass");
        let cases = [
            (
                SemanticPreference::DefaultModel,
                "fastembed",
                Some(FastEmbedder::default_model_dir(data_dir)),
            ),
            (SemanticPreference::HashFallback, "hash", None),
        ];

        for (preference, expected_backend, expected_model_dir) in cases {
            let surface = semantic_preference_surface(data_dir, preference);

            assert_eq!(surface.preferred_backend, expected_backend);
            assert_eq!(surface.model_dir, expected_model_dir);
        }
    }

    #[test]
    fn semantic_state_detects_progressive_and_hnsw_assets() {
        let temp = tempfile::tempdir().expect("tempdir");
        let vector_dir = temp.path().join(VECTOR_INDEX_DIR);
        std::fs::create_dir_all(&vector_dir).expect("create vector dir");
        std::fs::write(vector_dir.join("vector.fast.idx"), b"fast").expect("write fast tier");
        std::fs::write(vector_dir.join("vector.quality.idx"), b"quality")
            .expect("write quality tier");
        let hnsw_path = hnsw_index_path(temp.path(), FastEmbedder::embedder_id_static());
        std::fs::write(&hnsw_path, b"hnsw").expect("write hnsw");

        let state = semantic_state_from_availability(
            temp.path(),
            &SemanticAvailability::Ready {
                embedder_id: FastEmbedder::embedder_id_static().to_string(),
            },
            SemanticPreference::DefaultModel,
            None,
        );

        assert_eq!(state.status, "ready");
        assert!(state.progressive_ready);
        assert!(state.hnsw_ready);
        assert_eq!(
            state.embedder_id.as_deref(),
            Some(FastEmbedder::embedder_id_static())
        );
    }

    #[test]
    fn semantic_state_reports_backfill_when_manifest_only_has_stale_assets() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut manifest = SemanticManifest {
            fast_tier: Some(ArtifactRecord {
                tier: crate::search::semantic_manifest::TierKind::Fast,
                embedder_id: HashEmbedder::default().id().to_string(),
                model_revision: "hash".to_string(),
                schema_version: crate::search::policy::SEMANTIC_SCHEMA_VERSION,
                chunking_version: crate::search::policy::CHUNKING_STRATEGY_VERSION,
                dimension: 256,
                doc_count: 12,
                conversation_count: 3,
                db_fingerprint: "stale-db".to_string(),
                index_path: "vector_index/vector.fast.idx".to_string(),
                size_bytes: 4096,
                started_at_ms: 1_733_100_000_000,
                completed_at_ms: 1_733_100_100_000,
                ready: true,
            }),
            backlog: crate::search::semantic_manifest::BacklogLedger {
                total_conversations: 20,
                fast_tier_processed: 3,
                quality_tier_processed: 0,
                db_fingerprint: "current-db".to_string(),
                computed_at_ms: 1_733_100_200_000,
            },
            checkpoint: Some(BuildCheckpoint {
                tier: crate::search::semantic_manifest::TierKind::Fast,
                embedder_id: HashEmbedder::default().id().to_string(),
                last_offset: 77,
                docs_embedded: 66,
                conversations_processed: 3,
                total_conversations: 20,
                db_fingerprint: "current-db".to_string(),
                schema_version: crate::search::policy::SEMANTIC_SCHEMA_VERSION,
                chunking_version: crate::search::policy::CHUNKING_STRATEGY_VERSION,
                saved_at_ms: 1_733_100_300_000,
                last_message_id: None,
                cursor_exhausted: false,
            }),
            ..Default::default()
        };
        manifest.save(temp.path()).expect("save semantic manifest");

        let state = semantic_state_from_availability(
            temp.path(),
            &SemanticAvailability::NeedsConsent,
            SemanticPreference::DefaultModel,
            Some("current-db"),
        );

        assert_eq!(state.status, "building");
        assert_eq!(state.availability, "index_building");
        assert!(!state.can_search);
        assert_eq!(state.fallback_mode, Some("lexical"));
        assert!(state.summary.contains("backfill"));
        assert!(
            state
                .hint
                .as_deref()
                .is_some_and(|hint| hint.contains("finish backfilling"))
        );
    }

    #[test]
    fn semantic_state_prefers_current_hash_tier_over_missing_model() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut manifest = SemanticManifest {
            fast_tier: Some(ArtifactRecord {
                tier: crate::search::semantic_manifest::TierKind::Fast,
                embedder_id: HashEmbedder::default().id().to_string(),
                model_revision: "hash".to_string(),
                schema_version: crate::search::policy::SEMANTIC_SCHEMA_VERSION,
                chunking_version: crate::search::policy::CHUNKING_STRATEGY_VERSION,
                dimension: 256,
                doc_count: 12,
                conversation_count: 3,
                db_fingerprint: "current-db".to_string(),
                index_path: "vector_index/vector.fast.idx".to_string(),
                size_bytes: 4096,
                started_at_ms: 1_733_100_000_000,
                completed_at_ms: 1_733_100_100_000,
                ready: true,
            }),
            ..Default::default()
        };
        manifest.save(temp.path()).expect("save semantic manifest");
        let vector_path = vector_index_path(temp.path(), HashEmbedder::default().id());
        std::fs::create_dir_all(vector_path.parent().expect("vector parent"))
            .expect("create vector dir");
        std::fs::write(&vector_path, b"fast").expect("write fast vector index");

        let state = semantic_state_from_availability(
            temp.path(),
            &SemanticAvailability::NeedsConsent,
            SemanticPreference::DefaultModel,
            Some("current-db"),
        );

        assert_eq!(state.status, "ready");
        assert_eq!(state.availability, "ready");
        assert!(state.can_search);
        assert_eq!(state.fallback_mode, None);
        assert_eq!(
            state.embedder_id.as_deref(),
            Some(HashEmbedder::default().id())
        );
        assert_eq!(state.model_dir, None);
        assert_eq!(
            state.vector_index_path.as_deref(),
            Some(vector_path.as_path())
        );
        assert_eq!(state.hint, None);
    }

    #[test]
    fn semantic_state_treats_ready_quality_tier_with_unknown_db_match_as_queryable() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut manifest = SemanticManifest {
            quality_tier: Some(ArtifactRecord {
                tier: crate::search::semantic_manifest::TierKind::Quality,
                embedder_id: HashEmbedder::default().id().to_string(),
                model_revision: "hash".to_string(),
                schema_version: crate::search::policy::SEMANTIC_SCHEMA_VERSION,
                chunking_version: crate::search::policy::CHUNKING_STRATEGY_VERSION,
                dimension: 256,
                doc_count: 249,
                conversation_count: 21,
                db_fingerprint: "boxed-db".to_string(),
                index_path: "vector_index/vector.quality.idx".to_string(),
                size_bytes: 221_824,
                started_at_ms: 1_733_100_000_000,
                completed_at_ms: 1_733_100_100_000,
                ready: true,
            }),
            ..Default::default()
        };
        manifest.save(temp.path()).expect("save semantic manifest");

        let state = semantic_state_from_availability(
            temp.path(),
            &SemanticAvailability::NeedsConsent,
            SemanticPreference::DefaultModel,
            None,
        );

        assert_eq!(state.quality_tier.current_db_matches, None);
        assert!(
            state.quality_tier_published,
            "a ready quality tier with unknown DB match should remain visible as published in boxed data-dir status"
        );
        assert!(
            state.semantic_only_search_available,
            "semantic-only search can still run when the quality tier is ready and DB match is unknown"
        );
        assert!(state.can_search);
        assert_eq!(state.fallback_mode, None);
    }

    #[test]
    fn semantic_state_promotes_complete_current_shard_generation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let embedder_id = HashEmbedder::default().id().to_string();
        let mut records = Vec::new();
        for shard_index in 0..2_u32 {
            let relative_path = format!("vector_index/shards/fast-hash/shard-{shard_index}.fsvi");
            let path = temp.path().join(&relative_path);
            std::fs::create_dir_all(path.parent().expect("shard parent"))
                .expect("create shard parent");
            std::fs::write(&path, b"fsvi").expect("write shard placeholder");
            records.push(SemanticShardRecord {
                tier: TierKind::Fast,
                embedder_id: embedder_id.clone(),
                model_revision: "hash".to_string(),
                schema_version: SEMANTIC_SCHEMA_VERSION,
                chunking_version: CHUNKING_STRATEGY_VERSION,
                dimension: HashEmbedder::default().dimension(),
                shard_index,
                shard_count: 2,
                doc_count: 10 + u64::from(shard_index),
                total_conversations: 7,
                db_fingerprint: "current-db".to_string(),
                index_path: relative_path,
                quantization: "f16".to_string(),
                mmap_ready: true,
                ann_index_path: None,
                ann_size_bytes: 0,
                ann_ready: false,
                size_bytes: 100 + u64::from(shard_index),
                started_at_ms: 1_733_100_000_000,
                completed_at_ms: 1_733_100_000_000 + i64::from(shard_index),
                ready: true,
            });
        }
        let mut shards = SemanticShardManifest {
            shards: records,
            ..Default::default()
        };
        shards.save(temp.path()).expect("save shard manifest");

        let state = semantic_state_from_availability(
            temp.path(),
            &SemanticAvailability::IndexMissing {
                index_path: vector_index_path(temp.path(), &embedder_id),
            },
            SemanticPreference::HashFallback,
            Some("current-db"),
        );

        assert_eq!(state.status, "ready");
        assert_eq!(state.availability, "ready");
        assert!(state.can_search);
        assert_eq!(state.fallback_mode, None);
        assert_eq!(state.fast_tier.doc_count, Some(21));
        let expected_path = temp
            .path()
            .join("vector_index/shards/fast-hash/shard-0.fsvi");
        assert_eq!(
            state.vector_index_path.as_deref(),
            Some(expected_path.as_path())
        );
    }

    #[test]
    fn semantic_state_rejects_complete_shard_generation_with_unsafe_path() {
        let temp = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside tempdir");
        let outside_path = outside.path().join("outside.fsvi");
        std::fs::write(&outside_path, b"fsvi").expect("write outside placeholder");
        let embedder_id = HashEmbedder::default().id().to_string();
        let mut shards = SemanticShardManifest {
            shards: vec![SemanticShardRecord {
                tier: TierKind::Fast,
                embedder_id: embedder_id.clone(),
                model_revision: "hash".to_string(),
                schema_version: SEMANTIC_SCHEMA_VERSION,
                chunking_version: CHUNKING_STRATEGY_VERSION,
                dimension: HashEmbedder::default().dimension(),
                shard_index: 0,
                shard_count: 1,
                doc_count: 10,
                total_conversations: 7,
                db_fingerprint: "current-db".to_string(),
                index_path: outside_path.to_string_lossy().to_string(),
                quantization: "f16".to_string(),
                mmap_ready: true,
                ann_index_path: None,
                ann_size_bytes: 0,
                ann_ready: false,
                size_bytes: 100,
                started_at_ms: 1_733_100_000_000,
                completed_at_ms: 1_733_100_000_001,
                ready: true,
            }],
            ..Default::default()
        };
        shards.save(temp.path()).expect("save shard manifest");

        let base_vector_path = vector_index_path(temp.path(), &embedder_id);
        let state = semantic_state_from_availability(
            temp.path(),
            &SemanticAvailability::IndexMissing {
                index_path: base_vector_path.clone(),
            },
            SemanticPreference::HashFallback,
            Some("current-db"),
        );

        assert_ne!(state.status, "ready");
        assert!(!state.can_search);
        assert_eq!(
            state.vector_index_path.as_deref(),
            Some(base_vector_path.as_path())
        );
        assert_ne!(
            state.vector_index_path.as_deref(),
            Some(outside_path.as_path())
        );
    }

    // -----------------------------------------------------------------------
    // Maintenance coordination tests
    // -----------------------------------------------------------------------

    fn make_active_snapshot(now_ms: i64) -> SearchMaintenanceSnapshot {
        SearchMaintenanceSnapshot {
            active: true,
            pid: Some(12345),
            started_at_ms: Some(now_ms - 5_000),
            db_path: Some(PathBuf::from("/tmp/cass/agent_search.db")),
            mode: Some(SearchMaintenanceMode::Index),
            job_id: Some("lexical_refresh-1000-12345".to_string()),
            job_kind: Some(SearchMaintenanceJobKind::LexicalRefresh),
            phase: Some("scanning".to_string()),
            updated_at_ms: Some(now_ms - 500),
            last_progress_at_ms: Some(now_ms - 500),
            orphaned: false,
        }
    }

    #[test]
    fn coordination_no_active_job_when_snapshot_inactive() {
        let snapshot = SearchMaintenanceSnapshot::default();
        let outcome = evaluate_maintenance_coordination_from_snapshot(&snapshot, 1_733_000_000_000);
        assert_eq!(outcome, MaintenanceCoordinationOutcome::Idle);
    }

    #[test]
    fn coordination_tracks_active_legacy_lock_without_job_id() {
        let snapshot = SearchMaintenanceSnapshot {
            active: true,
            pid: Some(12345),
            job_id: None,
            mode: Some(SearchMaintenanceMode::Index),
            ..Default::default()
        };
        let outcome = evaluate_maintenance_coordination_from_snapshot(&snapshot, 1_733_000_000_000);
        if let MaintenanceCoordinationOutcome::Active {
            ref job_id,
            job_kind,
            ..
        } = outcome
        {
            assert_eq!(job_id, "index-active-lock-12345");
            assert_eq!(job_kind, SearchMaintenanceJobKind::LexicalRefresh);
        } else {
            assert!(
                matches!(outcome, MaintenanceCoordinationOutcome::Active { .. }),
                "legacy active lock must remain active, got {outcome:?}"
            );
        }
    }

    #[test]
    fn coordination_active_job_with_fresh_heartbeat() {
        let now_ms = 1_733_000_000_000i64;
        let snapshot = make_active_snapshot(now_ms);
        let outcome = evaluate_maintenance_coordination_from_snapshot(&snapshot, now_ms);
        if let MaintenanceCoordinationOutcome::Active {
            ref job_id,
            ref phase,
            ..
        } = outcome
        {
            assert_eq!(job_id, "lexical_refresh-1000-12345");
            assert_eq!(phase.as_deref(), Some("scanning"));
        } else {
            assert!(
                matches!(outcome, MaintenanceCoordinationOutcome::Active { .. }),
                "expected ActiveJob, got {outcome:?}"
            );
        }
    }

    #[test]
    fn coordination_stale_job_with_old_heartbeat() {
        let now_ms = 1_733_000_000_000i64;
        let snapshot = SearchMaintenanceSnapshot {
            updated_at_ms: Some(now_ms - 60_000),
            ..make_active_snapshot(now_ms)
        };
        let outcome = evaluate_maintenance_coordination_from_snapshot(&snapshot, now_ms);
        if let MaintenanceCoordinationOutcome::Stale {
            ref job_id,
            ref reason,
        } = outcome
        {
            assert_eq!(job_id, "lexical_refresh-1000-12345");
            assert!(reason.contains("60000ms"), "reason={reason}");
        } else {
            assert!(
                matches!(outcome, MaintenanceCoordinationOutcome::Stale { .. }),
                "expected StaleJob, got {outcome:?}"
            );
        }
    }

    #[test]
    fn coordination_missing_heartbeat_timestamp_still_respects_active_flock() {
        let now_ms = 1_733_000_000_000i64;
        let snapshot = SearchMaintenanceSnapshot {
            updated_at_ms: None,
            ..make_active_snapshot(now_ms)
        };
        let outcome = evaluate_maintenance_coordination_from_snapshot(&snapshot, now_ms);
        if let MaintenanceCoordinationOutcome::Active { updated_at_ms, .. } = outcome {
            assert_eq!(updated_at_ms, now_ms);
        } else {
            assert!(
                matches!(outcome, MaintenanceCoordinationOutcome::Active { .. }),
                "missing heartbeat metadata must not hide an active flock, got {outcome:?}"
            );
        }
    }

    #[test]
    fn decision_launch_when_no_job() {
        let snapshot = SearchMaintenanceSnapshot::default();
        let decision = decide_maintenance_action_from_snapshot(&snapshot, 1_733_000_000_000);
        assert_eq!(decision, MaintenanceDecision::Launch);
    }

    #[test]
    fn decision_launch_when_no_lock_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let decision = decide_maintenance_action(temp.path(), 1_733_000_000_000);
        assert_eq!(decision, MaintenanceDecision::Launch);
    }

    #[test]
    fn decision_attaches_when_active_lock_has_stale_heartbeat() {
        let now_ms = 1_733_000_000_000i64;
        let snapshot = SearchMaintenanceSnapshot {
            updated_at_ms: Some(now_ms - 60_000),
            ..make_active_snapshot(now_ms)
        };
        let decision = decide_maintenance_action_from_snapshot(&snapshot, now_ms);
        if let MaintenanceDecision::AttachOrWait {
            ref job_id,
            ref phase,
            elapsed_ms,
            ..
        } = decision
        {
            assert_eq!(job_id, "lexical_refresh-1000-12345");
            assert_eq!(phase.as_deref(), Some("scanning"));
            assert_eq!(elapsed_ms, 5_000);
        } else {
            assert!(
                matches!(decision, MaintenanceDecision::AttachOrWait { .. }),
                "stale heartbeat still has an active lock, got {decision:?}"
            );
        }
    }

    #[test]
    fn decision_attach_when_active_fresh_job() {
        let now_ms = 1_733_000_000_000i64;
        let snapshot = make_active_snapshot(now_ms);
        let decision = decide_maintenance_action_from_snapshot(&snapshot, now_ms);
        if let MaintenanceDecision::AttachOrWait {
            ref job_id,
            elapsed_ms,
            ..
        } = decision
        {
            assert_eq!(job_id, "lexical_refresh-1000-12345");
            assert_eq!(elapsed_ms, 5_000);
        } else {
            assert!(
                matches!(decision, MaintenanceDecision::AttachOrWait { .. }),
                "expected AttachOrWait, got {decision:?}"
            );
        }
    }

    #[test]
    fn poll_returns_immediately_when_no_active_job() {
        let temp = tempfile::tempdir().expect("tempdir");
        let result = poll_maintenance_until_idle(
            temp.path(),
            Some(Duration::from_millis(500)),
            Some(Duration::from_millis(50)),
        );
        assert!(!result.timed_out);
        assert_eq!(result.polls, 1);
        assert!(
            matches!(result.outcome, MaintenanceCoordinationOutcome::Idle),
            "expected NoActiveJob"
        );
        assert!(
            result.elapsed <= Duration::from_millis(500),
            "immediate idle poll should finish before timeout, elapsed={:?}",
            result.elapsed
        );
    }

    #[test]
    fn poll_returns_active_on_timeout_when_lock_held() {
        use fs2::FileExt;
        let temp = tempfile::tempdir().expect("tempdir");
        let lock_path = temp.path().join("index-run.lock");
        let now_ms = crate::storage::sqlite::FrankenStorage::now_millis();
        let pid = active_lock_fixture_pid(99999);
        let mut owner = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(&lock_path)
            .expect("open owner handle");
        owner.try_lock_exclusive().expect("acquire lock");
        write_locked_metadata(
            &lock_path,
            &mut owner,
            format!(
                "pid={pid}\nstarted_at_ms={}\nupdated_at_ms={}\ndb_path=/tmp/test.db\nmode=index\njob_id=test-job-1\njob_kind=lexical_refresh\nphase=scanning\n",
                now_ms - 1_000,
                now_ms,
            )
            .as_str(),
        )
        .expect("write lock metadata");

        let result = poll_maintenance_until_idle(
            temp.path(),
            Some(Duration::from_millis(300)),
            Some(Duration::from_millis(50)),
        );
        assert!(result.timed_out, "should time out when lock is held");
        assert!(result.polls >= 2, "should have polled multiple times");
        assert!(
            matches!(
                result.outcome,
                MaintenanceCoordinationOutcome::Active { .. }
            ),
            "expected ActiveJob on timeout"
        );

        let _ = FileExt::unlock(&owner);
    }

    #[test]
    fn poll_times_out_instead_of_declaring_stale_held_lock_idle() {
        use fs2::FileExt;
        let temp = tempfile::tempdir().expect("tempdir");
        let lock_path = temp.path().join("index-run.lock");
        let now_ms = crate::storage::sqlite::FrankenStorage::now_millis();
        let pid = active_lock_fixture_pid(99999);
        let mut owner = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(&lock_path)
            .expect("open owner handle");
        owner.try_lock_exclusive().expect("acquire lock");
        write_locked_metadata(
            &lock_path,
            &mut owner,
            format!(
                "pid={pid}\nstarted_at_ms={}\nupdated_at_ms={}\ndb_path=/tmp/test.db\nmode=index\njob_id=test-job-stale\njob_kind=lexical_refresh\nphase=scanning\n",
                now_ms - 120_000,
                now_ms - 120_000,
            )
            .as_str(),
        )
        .expect("write lock metadata");

        let result = poll_maintenance_until_idle(
            temp.path(),
            Some(Duration::from_millis(150)),
            Some(Duration::from_millis(25)),
        );
        assert!(result.timed_out, "held stale lock is still not idle");
        assert!(
            matches!(result.outcome, MaintenanceCoordinationOutcome::Stale { .. }),
            "expected stale held lock on timeout, got {:?}",
            result.outcome
        );

        let _ = FileExt::unlock(&owner);
    }

    #[test]
    fn poll_detects_release_mid_wait() {
        use fs2::FileExt;
        let temp = tempfile::tempdir().expect("tempdir");
        let lock_path = temp.path().join("index-run.lock");
        let now_ms = crate::storage::sqlite::FrankenStorage::now_millis();
        let pid = active_lock_fixture_pid(99999);
        let mut owner = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(&lock_path)
            .expect("open owner handle");
        owner.try_lock_exclusive().expect("acquire lock");
        write_locked_metadata(
            &lock_path,
            &mut owner,
            format!(
                "pid={pid}\nstarted_at_ms={}\nupdated_at_ms={}\ndb_path=/tmp/test.db\nmode=index\njob_id=test-job-2\njob_kind=lexical_refresh\nphase=committing\n",
                now_ms - 1_000,
                now_ms,
            )
            .as_str(),
        )
        .expect("write lock metadata");

        let temp_path = temp.path().to_path_buf();
        let lock_path_for_release = lock_path.clone();
        let release_thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            let _ = owner.set_len(0);
            let _ = clear_index_run_lock_metadata_sidecar(&lock_path_for_release);
            let _ = FileExt::unlock(&owner);
            drop(owner);
        });

        let result = poll_maintenance_until_idle(
            &temp_path,
            Some(Duration::from_secs(2)),
            Some(Duration::from_millis(50)),
        );
        assert!(!result.timed_out, "should detect release before timeout");
        release_thread.join().expect("release thread");
    }

    #[test]
    fn failopen_returns_failopen_when_lexical_available_and_job_active() {
        let temp = tempfile::tempdir().expect("tempdir");
        let lock_path = temp.path().join("index-run.lock");
        let now_ms = crate::storage::sqlite::FrankenStorage::now_millis();
        let pid = active_lock_fixture_pid(99999);

        use fs2::FileExt;
        let mut owner = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(&lock_path)
            .expect("open owner handle");
        owner.try_lock_exclusive().expect("acquire lock");
        write_locked_metadata(
            &lock_path,
            &mut owner,
            format!(
                "pid={pid}\nstarted_at_ms={}\nupdated_at_ms={}\ndb_path=/tmp/test.db\nmode=index\njob_id=fo-job-1\njob_kind=lexical_refresh\nphase=indexing\n",
                now_ms - 1_000,
                now_ms,
            )
            .as_str(),
        )
        .expect("write lock metadata");

        let decision = decide_search_failopen(temp.path(), now_ms, true);
        if let MaintenanceDecision::FailOpen { ref reason } = decision {
            assert!(reason.contains("fo-job-1"), "reason={reason}");
            assert!(reason.contains("failing open"), "reason={reason}");
        } else {
            assert!(
                matches!(decision, MaintenanceDecision::FailOpen { .. }),
                "expected FailOpen, got {decision:?}"
            );
        }

        let decision_no_lexical = decide_search_failopen(temp.path(), now_ms, false);
        assert!(
            matches!(
                decision_no_lexical,
                MaintenanceDecision::AttachOrWait { .. }
            ),
            "without lexical must attach, got {decision_no_lexical:?}"
        );

        let _ = FileExt::unlock(&owner);
    }

    #[test]
    fn failopen_handles_active_stale_heartbeat_without_launching_repair() {
        let temp = tempfile::tempdir().expect("tempdir");
        let lock_path = temp.path().join("index-run.lock");
        let now_ms = crate::storage::sqlite::FrankenStorage::now_millis();
        let pid = active_lock_fixture_pid(99999);

        use fs2::FileExt;
        let mut owner = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(&lock_path)
            .expect("open owner handle");
        owner.try_lock_exclusive().expect("acquire lock");
        write_locked_metadata(
            &lock_path,
            &mut owner,
            format!(
                "pid={pid}\nstarted_at_ms={}\nupdated_at_ms={}\ndb_path=/tmp/test.db\nmode=index\njob_id=fo-stale-1\njob_kind=lexical_refresh\nphase=indexing\n",
                now_ms - 120_000,
                now_ms - 120_000,
            )
            .as_str(),
        )
        .expect("write lock metadata");

        let decision = decide_search_failopen(temp.path(), now_ms, true);
        if let MaintenanceDecision::FailOpen { ref reason } = decision {
            assert!(reason.contains("fo-stale-1"), "reason={reason}");
            assert!(reason.contains("stale heartbeat"), "reason={reason}");
            assert!(reason.contains("failing open"), "reason={reason}");
        } else {
            assert!(
                matches!(decision, MaintenanceDecision::FailOpen { .. }),
                "expected FailOpen for searchable stale active lock, got {decision:?}"
            );
        }

        let decision_no_lexical = decide_search_failopen(temp.path(), now_ms, false);
        assert!(
            matches!(
                decision_no_lexical,
                MaintenanceDecision::AttachOrWait { .. }
            ),
            "without lexical must wait for the held lock, got {decision_no_lexical:?}"
        );

        let _ = FileExt::unlock(&owner);
    }

    // -----------------------------------------------------------------------
    // ibuuh.22: Event log, yield signaling, unified view tests
    // -----------------------------------------------------------------------

    #[test]
    fn event_log_append_and_read() {
        let temp = tempfile::tempdir().expect("tempdir");
        let event = MaintenanceEvent {
            timestamp_ms: 1_733_000_000_000,
            job_id: "test-job-1".to_string(),
            actor_pid: 42,
            kind: MaintenanceEventKind::Started {
                job_kind: "lexical_refresh".to_string(),
                phase: "scanning".to_string(),
            },
        };
        append_maintenance_event(temp.path(), &event).expect("append");
        let events = read_maintenance_events(temp.path(), None, None);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].job_id, "test-job-1");
        assert_eq!(events[0].actor_pid, 42);
        assert!(matches!(
            events[0].kind,
            MaintenanceEventKind::Started { .. }
        ));
    }

    #[test]
    fn event_log_filters_by_timestamp() {
        let temp = tempfile::tempdir().expect("tempdir");
        for i in 0..5 {
            let event = MaintenanceEvent {
                timestamp_ms: 1_000 + i,
                job_id: format!("job-{i}"),
                actor_pid: 1,
                kind: MaintenanceEventKind::Progress {
                    processed: i as u64,
                    total: 5,
                },
            };
            append_maintenance_event(temp.path(), &event).expect("append");
        }
        let events = read_maintenance_events(temp.path(), Some(1_002), None);
        assert_eq!(events.len(), 2, "should only get events after ts 1002");
        assert_eq!(events[0].timestamp_ms, 1_003);
        assert_eq!(events[1].timestamp_ms, 1_004);
    }

    #[test]
    fn event_log_respects_limit() {
        let temp = tempfile::tempdir().expect("tempdir");
        for i in 0..10 {
            let event = MaintenanceEvent {
                timestamp_ms: 1_000 + i,
                job_id: format!("job-{i}"),
                actor_pid: 1,
                kind: MaintenanceEventKind::Resumed,
            };
            append_maintenance_event(temp.path(), &event).expect("append");
        }
        let events = read_maintenance_events(temp.path(), None, Some(3));
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].timestamp_ms, 1_007);
        assert_eq!(events[2].timestamp_ms, 1_009);
    }

    #[test]
    fn event_log_returns_empty_when_missing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let events = read_maintenance_events(temp.path(), None, None);
        assert!(events.is_empty());
    }

    #[test]
    fn event_log_truncation_retains_tail() {
        let temp = tempfile::tempdir().expect("tempdir");
        for i in 0..550 {
            let event = MaintenanceEvent {
                timestamp_ms: i,
                job_id: format!("job-{i}"),
                actor_pid: 1,
                kind: MaintenanceEventKind::Resumed,
            };
            append_maintenance_event(temp.path(), &event).expect("append");
        }
        let before = read_maintenance_events(temp.path(), None, Some(600));
        assert_eq!(before.len(), 550);
        truncate_maintenance_event_log(temp.path()).expect("truncate");
        let after = read_maintenance_events(temp.path(), None, Some(600));
        assert_eq!(after.len(), MAX_EVENT_LOG_ENTRIES);
        assert_eq!(after[0].timestamp_ms, 50);
        assert_eq!(after[MAX_EVENT_LOG_ENTRIES - 1].timestamp_ms, 549);
    }

    #[test]
    fn yield_signal_round_trip() {
        let temp = tempfile::tempdir().expect("tempdir");
        assert!(
            check_yield_requested(temp.path()).is_none(),
            "no signal initially"
        );
        request_yield(temp.path(), "foreground search pressure").expect("request yield");
        let req = check_yield_requested(temp.path()).expect("yield should be present");
        assert_eq!(req.requester_pid, std::process::id());
        assert_eq!(req.reason, "foreground search pressure");
        assert!(req.requested_at_ms > 0);
        clear_yield_signal(temp.path()).expect("clear");
        assert!(
            check_yield_requested(temp.path()).is_none(),
            "signal cleared"
        );
    }

    #[test]
    fn clear_yield_signal_is_idempotent() {
        let temp = tempfile::tempdir().expect("tempdir");
        clear_yield_signal(temp.path()).expect("clear nonexistent");
        clear_yield_signal(temp.path()).expect("clear again");
    }

    #[test]
    fn unified_view_idle_no_events() {
        let temp = tempfile::tempdir().expect("tempdir");
        let view = unified_maintenance_view(temp.path(), true);
        assert!(matches!(
            view.coordination,
            MaintenanceCoordinationOutcome::Idle
        ));
        assert!(view.yield_pending.is_none());
        assert!(view.recent_events.is_empty());
        assert_eq!(view.decision, MaintenanceDecision::Launch);
    }

    #[test]
    fn unified_view_active_with_lexical_fails_open() {
        use fs2::FileExt;
        let temp = tempfile::tempdir().expect("tempdir");
        let lock_path = temp.path().join("index-run.lock");
        let now_ms = crate::storage::sqlite::FrankenStorage::now_millis();
        let pid = active_lock_fixture_pid(99999);
        let mut owner = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(&lock_path)
            .expect("open");
        owner.try_lock_exclusive().expect("lock");
        write_locked_metadata(
            &lock_path,
            &mut owner,
            format!(
                "pid={pid}\nstarted_at_ms={}\nupdated_at_ms={}\ndb_path=/tmp/t.db\nmode=index\njob_id=uv-1\njob_kind=lexical_refresh\nphase=indexing\n",
                now_ms - 1_000,
                now_ms,
            )
            .as_str(),
        )
        .expect("write metadata");

        let event = MaintenanceEvent {
            timestamp_ms: now_ms,
            job_id: "uv-1".to_string(),
            actor_pid: pid,
            kind: MaintenanceEventKind::Started {
                job_kind: "lexical_refresh".to_string(),
                phase: "indexing".to_string(),
            },
        };
        append_maintenance_event(temp.path(), &event).expect("append");

        let view = unified_maintenance_view(temp.path(), true);
        assert!(matches!(
            view.coordination,
            MaintenanceCoordinationOutcome::Active { .. }
        ));
        assert!(matches!(
            view.decision,
            MaintenanceDecision::FailOpen { .. }
        ));
        assert_eq!(view.recent_events.len(), 1);

        let _ = FileExt::unlock(&owner);
    }

    #[test]
    fn unified_view_includes_yield_signal() {
        let temp = tempfile::tempdir().expect("tempdir");
        request_yield(temp.path(), "test yield").expect("yield");
        let view = unified_maintenance_view(temp.path(), true);
        assert!(view.yield_pending.is_some());
        assert_eq!(view.yield_pending.as_ref().unwrap().reason, "test yield");
        clear_yield_signal(temp.path()).expect("clear");
    }

    #[test]
    fn event_kinds_serialize_round_trip() {
        let temp = tempfile::tempdir().expect("tempdir");
        let kinds = vec![
            MaintenanceEventKind::Started {
                job_kind: "lexical_refresh".to_string(),
                phase: "init".to_string(),
            },
            MaintenanceEventKind::PhaseChanged {
                from: "init".to_string(),
                to: "scanning".to_string(),
            },
            MaintenanceEventKind::Progress {
                processed: 50,
                total: 100,
            },
            MaintenanceEventKind::YieldRequested {
                requester_pid: 42,
                reason: "foreground".to_string(),
            },
            MaintenanceEventKind::Paused {
                reason: "yield".to_string(),
            },
            MaintenanceEventKind::Resumed,
            MaintenanceEventKind::Completed {
                summary: "done".to_string(),
            },
            MaintenanceEventKind::Failed {
                error: "oops".to_string(),
            },
            MaintenanceEventKind::Cancelled {
                reason: "user".to_string(),
            },
        ];
        for (i, kind) in kinds.into_iter().enumerate() {
            let event = MaintenanceEvent {
                timestamp_ms: i as i64,
                job_id: "rt-test".to_string(),
                actor_pid: 1,
                kind,
            };
            append_maintenance_event(temp.path(), &event).expect("append");
        }
        let events = read_maintenance_events(temp.path(), None, None);
        assert_eq!(events.len(), 9);
        assert!(matches!(
            events[0].kind,
            MaintenanceEventKind::Started { .. }
        ));
        assert!(matches!(events[5].kind, MaintenanceEventKind::Resumed));
        assert!(matches!(
            events[8].kind,
            MaintenanceEventKind::Cancelled { .. }
        ));
    }
}
