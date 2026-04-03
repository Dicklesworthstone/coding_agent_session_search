//! Shared search asset state evaluation for status, health, and fail-open search planning.
//!
//! This module centralizes coarse-grained asset truth so callers stop inferring
//! lexical freshness, active maintenance, and semantic readiness from ad hoc
//! file checks spread across the CLI.

use std::fs::OpenOptions;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fs2::FileExt;

use crate::indexer::{
    LEXICAL_REBUILD_PAGE_SIZE_PUBLIC, LexicalRebuildCheckpoint, lexical_storage_fingerprint_for_db,
    load_lexical_rebuild_checkpoint,
};
use crate::search::ann_index::hnsw_index_path;
use crate::search::embedder::Embedder;
use crate::search::fastembed_embedder::FastEmbedder;
use crate::search::hash_embedder::HashEmbedder;
use crate::search::model_manager::{
    SemanticAvailability, load_hash_semantic_context, load_semantic_context,
};
use crate::search::tantivy::{SCHEMA_HASH, index_dir};
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
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
    pub orphaned: bool,
}

pub(crate) fn read_search_maintenance_snapshot(data_dir: &Path) -> SearchMaintenanceSnapshot {
    let lock_path = data_dir.join("index-run.lock");
    let mut file = match OpenOptions::new().read(true).write(true).open(&lock_path) {
        Ok(file) => file,
        Err(_) => return SearchMaintenanceSnapshot::default(),
    };

    let mut raw = String::new();
    let _ = file.read_to_string(&mut raw);

    let mut pid = None;
    let mut started_at_ms = None;
    let mut lock_db_path = None::<PathBuf>;
    let mut mode = None;
    let mut job_id = None;
    let mut job_kind = None;
    let mut phase = None;
    let mut updated_at_ms = None;
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
            _ => {}
        }
    }

    let active = match file.try_lock_exclusive() {
        Ok(()) => {
            let _ = file.unlock();
            false
        }
        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => true,
        Err(_) => false,
    };
    let metadata_present = pid.is_some()
        || started_at_ms.is_some()
        || lock_db_path.is_some()
        || mode.is_some()
        || job_id.is_some()
        || job_kind.is_some()
        || phase.is_some()
        || updated_at_ms.is_some();

    SearchMaintenanceSnapshot {
        active,
        pid,
        started_at_ms,
        db_path: lock_db_path,
        mode,
        job_id,
        job_kind,
        phase,
        updated_at_ms,
        orphaned: metadata_present && !active,
    }
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
    pub hint: Option<String>,
}

pub(crate) struct InspectSearchAssetsInput<'a> {
    pub data_dir: &'a Path,
    pub db_path: &'a Path,
    pub stale_threshold: u64,
    pub last_indexed_at_ms: Option<i64>,
    pub now_secs: u64,
    pub maintenance: SearchMaintenanceSnapshot,
    pub semantic_preference: SemanticPreference,
    pub db_available: bool,
}

pub(crate) fn inspect_search_assets(
    input: InspectSearchAssetsInput<'_>,
) -> Result<SearchAssetSnapshot> {
    let InspectSearchAssetsInput {
        data_dir,
        db_path,
        stale_threshold,
        last_indexed_at_ms,
        now_secs,
        maintenance,
        semantic_preference,
        db_available,
    } = input;

    Ok(SearchAssetSnapshot {
        lexical: inspect_lexical_assets(
            data_dir,
            db_path,
            stale_threshold,
            last_indexed_at_ms,
            now_secs,
            maintenance,
            db_available,
        )?,
        semantic: inspect_semantic_assets(data_dir, db_path, semantic_preference),
    })
}

pub(crate) fn inspect_semantic_assets(
    data_dir: &Path,
    db_path: &Path,
    preference: SemanticPreference,
) -> SemanticAssetState {
    let setup = match preference {
        SemanticPreference::DefaultModel => load_semantic_context(data_dir, db_path),
        SemanticPreference::HashFallback => load_hash_semantic_context(data_dir, db_path),
    };
    semantic_state_from_availability(data_dir, &setup.availability, preference)
}

pub(crate) fn semantic_state_from_availability(
    data_dir: &Path,
    availability: &SemanticAvailability,
    preference: SemanticPreference,
) -> SemanticAssetState {
    let preferred_backend = match preference {
        SemanticPreference::DefaultModel => "fastembed",
        SemanticPreference::HashFallback => "hash",
    };
    let embedder_id = semantic_embedder_id(availability, preference);
    let vector_index_path = semantic_vector_index_path(data_dir, availability, preference);
    let model_dir = match preference {
        SemanticPreference::DefaultModel => Some(FastEmbedder::default_model_dir(data_dir)),
        SemanticPreference::HashFallback => None,
    };
    let hnsw_path = embedder_id
        .as_deref()
        .map(|embedder_id| hnsw_index_path(data_dir, embedder_id));
    let hnsw_ready = hnsw_path.as_ref().is_some_and(|path| path.is_file());
    let progressive_ready = semantic_progressive_assets_ready(data_dir);
    let availability_code = semantic_availability_code(availability);
    let status = semantic_status_from_availability(availability);
    let summary = availability.summary();
    let can_search = availability.can_search();
    let hint = semantic_hint(availability, preference);

    SemanticAssetState {
        status,
        availability: availability_code,
        summary,
        available: can_search,
        can_search,
        fallback_mode: (!can_search).then_some("lexical"),
        preferred_backend,
        embedder_id,
        vector_index_path,
        model_dir,
        hnsw_path,
        hnsw_ready,
        progressive_ready,
        hint,
    }
}

fn inspect_lexical_assets(
    data_dir: &Path,
    db_path: &Path,
    stale_threshold: u64,
    last_indexed_at_ms: Option<i64>,
    now_secs: u64,
    maintenance: SearchMaintenanceSnapshot,
    db_available: bool,
) -> Result<LexicalAssetState> {
    let index_path = index_dir(data_dir).unwrap_or_else(|_| data_dir.join("index").join("v4"));
    let checkpoint = load_lexical_rebuild_checkpoint(&index_path)
        .with_context(|| format!("loading lexical checkpoint from {}", index_path.display()))?;
    let current_db_fingerprint = if db_available {
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
        now_secs,
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
    now_secs: u64,
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
        now_secs,
        maintenance,
        checkpoint,
        current_db_fingerprint,
    } = input;
    let exists = index_path.exists();
    let checkpoint_db_matches = checkpoint.map(|state| state.db_path == db_path.to_string_lossy());
    let schema_matches = checkpoint.map(|state| state.schema_hash == SCHEMA_HASH);
    let page_size_matches =
        checkpoint.map(|state| state.page_size == LEXICAL_REBUILD_PAGE_SIZE_PUBLIC);
    let checkpoint_fingerprint = checkpoint.map(|state| state.storage_fingerprint.as_str());
    let fingerprint_matches = match (current_db_fingerprint, checkpoint_fingerprint) {
        (Some(current), Some(saved)) => Some(current == saved),
        _ => None,
    };
    let checkpoint_incomplete = checkpoint.is_some_and(|state| !state.completed);
    let contract_mismatch = schema_matches == Some(false) || page_size_matches == Some(false);
    let fingerprint_mismatch = fingerprint_matches == Some(false);
    let age_seconds = last_indexed_at_ms
        .and_then(|ts| (ts > 0).then(|| now_secs.saturating_sub((ts / 1000) as u64)));
    let age_stale = match age_seconds {
        Some(age) => age > stale_threshold,
        None => true,
    };
    let maintenance_targets_current_db = maintenance
        .db_path
        .as_ref()
        .is_none_or(|lock_db_path| lock_db_path == db_path);
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
    let stale = if rebuilding {
        !exists || contract_mismatch
    } else {
        !exists || age_stale || checkpoint_incomplete || contract_mismatch || fingerprint_mismatch
    };
    let fresh = exists && !stale && !rebuilding;
    let status = if rebuilding {
        "building"
    } else if !exists {
        "missing"
    } else if stale {
        "stale"
    } else {
        "ready"
    };
    let status_reason = if rebuilding {
        Some("lexical rebuild is in progress".to_string())
    } else if !exists {
        Some("lexical index directory missing".to_string())
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
        && page_size_matches == Some(true)
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
    let activity_at_ms = checkpoint
        .filter(|_| checkpoint_progress_usable)
        .and_then(|state| (state.updated_at_ms > 0).then_some(state.updated_at_ms))
        .or(maintenance_activity_at_ms);

    LexicalAssetState {
        status,
        exists,
        fresh,
        stale,
        rebuilding,
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
            "Run 'cass index --semantic --embedder hash' to build the hash vector index, or use --mode lexical"
        }
        (SemanticPreference::HashFallback, SemanticAvailability::LoadFailed { .. })
        | (SemanticPreference::HashFallback, SemanticAvailability::DatabaseUnavailable { .. }) => {
            "Run 'cass index --semantic --embedder hash' after the database is healthy, or use --mode lexical"
        }
        (SemanticPreference::HashFallback, _) => {
            "Run 'cass index --semantic --embedder hash' to build the hash vector index, or use --mode lexical"
        }
        (_, SemanticAvailability::NotInstalled)
        | (_, SemanticAvailability::NeedsConsent)
        | (_, SemanticAvailability::ModelMissing { .. }) => {
            "Run 'cass models install' and then 'cass index --semantic', or use --mode lexical"
        }
        (_, SemanticAvailability::IndexMissing { .. })
        | (_, SemanticAvailability::UpdateAvailable { .. })
        | (_, SemanticAvailability::IndexBuilding { .. }) => {
            "Run 'cass index --semantic' to build or refresh vector assets, or use --mode lexical"
        }
        (_, SemanticAvailability::Downloading { .. }) | (_, SemanticAvailability::Verifying) => {
            "Wait for the semantic model installation to finish, or use --mode lexical"
        }
        (_, SemanticAvailability::Disabled { .. }) => {
            "Semantic search is disabled by policy; use --mode lexical or re-enable semantic search"
        }
        (_, SemanticAvailability::DatabaseUnavailable { .. })
        | (_, SemanticAvailability::LoadFailed { .. }) => {
            "Restore the semantic assets and database, or use --mode lexical"
        }
        (_, SemanticAvailability::Ready { .. }) | (_, SemanticAvailability::HashFallback) => {
            return None;
        }
    };
    Some(hint.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn inactive_lock_metadata_is_reported_as_orphaned() {
        let temp = tempfile::tempdir().expect("tempdir");
        let lock_path = temp.path().join("index-run.lock");
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
        assert!(!snapshot.active);
        assert!(snapshot.orphaned);
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
    }

    #[test]
    fn lexical_state_marks_fingerprint_mismatch_stale() {
        let temp = tempfile::tempdir().expect("tempdir");
        let index_path = temp.path().join("index").join("v4");
        std::fs::create_dir_all(&index_path).expect("create index dir");
        let db_path = temp.path().join("agent_search.db");
        std::fs::write(&db_path, b"db").expect("write db file");

        let checkpoint = LexicalRebuildCheckpoint {
            db_path: db_path.display().to_string(),
            total_conversations: 10,
            storage_fingerprint: "before".to_string(),
            committed_offset: 10,
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
            now_secs: 1_733_000_001,
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
    fn lexical_state_keeps_progress_visible_during_active_rebuild_despite_fingerprint_drift() {
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
            now_secs: 1_733_000_001,
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
        assert_eq!(
            state.status_reason.as_deref(),
            Some("lexical rebuild is in progress")
        );
    }

    #[test]
    fn lexical_state_ignores_rebuild_lock_for_different_database() {
        let temp = tempfile::tempdir().expect("tempdir");
        let index_path = temp.path().join("index").join("v4");
        std::fs::create_dir_all(&index_path).expect("create index dir");
        let db_path = temp.path().join("agent_search.db");
        std::fs::write(&db_path, b"db").expect("write db file");
        let other_db_path = temp.path().join("other.db");
        std::fs::write(&other_db_path, b"other").expect("write other db file");

        let checkpoint = LexicalRebuildCheckpoint {
            db_path: db_path.display().to_string(),
            total_conversations: 10,
            storage_fingerprint: "before".to_string(),
            committed_offset: 4,
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
            now_secs: 1_733_000_001,
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
        let db_path = temp.path().join("agent_search.db");
        std::fs::write(&db_path, b"db").expect("write db file");
        let other_db_path = temp.path().join("other.db");
        std::fs::write(&other_db_path, b"other").expect("write other db file");

        let state = lexical_state_from_observations(LexicalObservationInput {
            index_path: &index_path,
            db_path: &db_path,
            stale_threshold: 60,
            last_indexed_at_ms: Some(1_733_000_000_000),
            now_secs: 1_733_000_020,
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
            now_secs: 1_733_000_001,
            maintenance: SearchMaintenanceSnapshot::default(),
            semantic_preference: SemanticPreference::HashFallback,
            db_available: false,
        })
        .expect("asset inspection should not fail when db availability is already known");

        assert_ne!(snapshot.lexical.status, "error");
        assert_eq!(snapshot.semantic.status, "error");
        assert_eq!(snapshot.semantic.availability, "database_unavailable");
        assert_eq!(snapshot.semantic.fallback_mode, Some("lexical"));
        assert!(snapshot.semantic.summary.contains("db unavailable"));
    }

    #[test]
    fn semantic_state_reports_hash_fallback_as_searchable() {
        let state = semantic_state_from_availability(
            Path::new("/tmp/cass"),
            &SemanticAvailability::HashFallback,
            SemanticPreference::HashFallback,
        );

        assert_eq!(state.status, "hash_fallback");
        assert_eq!(state.availability, "hash_fallback");
        assert!(state.available);
        assert!(state.can_search);
        assert_eq!(state.fallback_mode, None);
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
        );

        assert_eq!(state.status, "ready");
        assert!(state.progressive_ready);
        assert!(state.hnsw_ready);
        assert_eq!(
            state.embedder_id.as_deref(),
            Some(FastEmbedder::embedder_id_static())
        );
    }
}
