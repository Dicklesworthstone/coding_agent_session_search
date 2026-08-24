//! Semantic model management (local-only detection).
//!
//! This module wires the pure-Rust native MiniLM embedder into semantic search by:
//! - validating the local model files
//! - loading the vector index
//! - building filter maps from the SQLite database
//! - detecting model version mismatches
//!
//! It does **not** download models. Missing files are surfaced as availability
//! states so the UI can guide the user. Downloads are handled by [`model_download`].

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::indexer::semantic::expected_vector_space_revision;
use crate::search::ann_index::hnsw_index_path;
use crate::search::embedder::Embedder;
use crate::search::fastembed_embedder::{FastEmbedder, LazyFastEmbedder};
use crate::search::hash_embedder::HashEmbedder;
use crate::search::model_download::{
    ModelAcquisitionPolicy, ModelCacheState, ModelManifest, classify_model_cache,
    classify_model_cache_metadata,
};
use crate::search::policy::{CliSemanticOverrides, SemanticPolicy};
use crate::search::semantic_manifest::{
    SemanticCurrentPointerV1, SemanticShardManifest, SemanticShardRecord, TierKind,
    semantic_shard_artifact_path_is_safe,
};
use crate::search::vector_index::{
    ROLE_ASSISTANT, ROLE_USER, SemanticFilterMaps, SemanticIndexArtifact,
    SemanticProgressiveUnavailableReason, VectorIndex, vector_index_path,
};
use crate::storage::sqlite::FrankenStorage;

/// Unified TUI state machine for semantic search availability.
///
/// This enum tracks the full lifecycle of semantic search from the user's perspective:
/// - Model installation flow (NotInstalled → NeedsConsent → Downloading → Verifying → Ready)
/// - Index building flow (Ready → IndexBuilding → Ready)
/// - Explicit user preferences (HashFallback, Disabled)
/// - Error states (LoadFailed, ModelMissing, etc.)
#[derive(Debug, Clone)]
pub enum SemanticAvailability {
    /// Model is ready for use.
    Ready { embedder_id: String },

    // =========================================================================
    // TUI-centric states for user flow
    // =========================================================================
    /// Model not installed - semantic not available.
    /// TUI should show an option to download or explicitly choose hash mode.
    NotInstalled,

    /// User needs to consent before downloading model.
    /// TUI should show consent dialog.
    NeedsConsent,

    /// Model download in progress.
    Downloading {
        /// Progress percentage (0-100).
        progress_pct: u8,
        /// Bytes downloaded so far.
        bytes_downloaded: u64,
        /// Total bytes to download.
        total_bytes: u64,
    },

    /// Verifying downloaded model (SHA256 check).
    Verifying,

    /// Index is being built or rebuilt.
    IndexBuilding {
        embedder_id: String,
        /// Optional progress percentage (0-100).
        progress_pct: Option<u8>,
        /// Number of items indexed so far.
        items_indexed: u64,
        /// Total items to index.
        total_items: u64,
    },

    /// Vector assets load, but the semantic manifest says they do not describe
    /// the current database (stale generation, or a backfill that has not yet
    /// published a queryable tier). Serving them would return nearest
    /// neighbours of an outdated corpus, so the query path must not consult
    /// them (GH #404). `reason` carries the manifest-derived verdict.
    IndexStale { embedder_id: String, reason: String },

    /// User explicitly opted for hash-based degraded mode (no ML model).
    HashFallback,

    /// Semantic search disabled by policy or user.
    Disabled { reason: String },

    // =========================================================================
    // Diagnostic states for troubleshooting
    // =========================================================================
    /// Model files are missing.
    ModelMissing {
        model_dir: PathBuf,
        missing_files: Vec<String>,
    },

    /// Vector index is missing.
    IndexMissing { index_path: PathBuf },

    /// Database is unavailable.
    DatabaseUnavailable { db_path: PathBuf, error: String },

    /// Failed to load semantic context.
    LoadFailed { context: String },

    /// Model update available - index rebuild needed.
    UpdateAvailable {
        embedder_id: String,
        current_revision: String,
        latest_revision: String,
    },
}

impl SemanticAvailability {
    /// Check if semantic search is ready to use.
    pub fn is_ready(&self) -> bool {
        matches!(self, SemanticAvailability::Ready { .. })
    }

    /// Check if a model update is available.
    pub fn has_update(&self) -> bool {
        matches!(self, SemanticAvailability::UpdateAvailable { .. })
    }

    /// Check if the index is being rebuilt.
    pub fn is_building(&self) -> bool {
        matches!(self, SemanticAvailability::IndexBuilding { .. })
    }

    /// Check if loadable vector assets were refused because they do not
    /// describe the current database (GH #404).
    pub fn is_index_stale(&self) -> bool {
        matches!(self, SemanticAvailability::IndexStale { .. })
    }

    /// Check if a download is in progress.
    pub fn is_downloading(&self) -> bool {
        matches!(self, SemanticAvailability::Downloading { .. })
    }

    /// Check if user consent is needed.
    pub fn needs_consent(&self) -> bool {
        matches!(self, SemanticAvailability::NeedsConsent)
    }

    /// Check if explicit hash mode is active.
    pub fn is_hash_fallback(&self) -> bool {
        matches!(self, SemanticAvailability::HashFallback)
    }

    /// Check if semantic search is disabled.
    pub fn is_disabled(&self) -> bool {
        matches!(self, SemanticAvailability::Disabled { .. })
    }

    /// Check if the model is not installed.
    pub fn is_not_installed(&self) -> bool {
        matches!(
            self,
            SemanticAvailability::NotInstalled | SemanticAvailability::ModelMissing { .. }
        )
    }

    /// Check if any error state is active.
    pub fn is_error(&self) -> bool {
        matches!(
            self,
            SemanticAvailability::LoadFailed { .. }
                | SemanticAvailability::DatabaseUnavailable { .. }
        )
    }

    /// Check if vector search can be used (native semantic or explicit hash mode).
    pub fn can_search(&self) -> bool {
        matches!(
            self,
            SemanticAvailability::Ready { .. } | SemanticAvailability::HashFallback
        )
    }

    /// Get download progress if downloading.
    pub fn download_progress(&self) -> Option<(u8, u64, u64)> {
        match self {
            SemanticAvailability::Downloading {
                progress_pct,
                bytes_downloaded,
                total_bytes,
            } => Some((*progress_pct, *bytes_downloaded, *total_bytes)),
            _ => None,
        }
    }

    /// Get index building progress if building.
    pub fn index_progress(&self) -> Option<(Option<u8>, u64, u64)> {
        match self {
            SemanticAvailability::IndexBuilding {
                progress_pct,
                items_indexed,
                total_items,
                ..
            } => Some((*progress_pct, *items_indexed, *total_items)),
            _ => None,
        }
    }

    /// Get a short status label for display in status bar.
    pub fn status_label(&self) -> &'static str {
        match self {
            SemanticAvailability::Ready { .. } => "SEM",
            SemanticAvailability::HashFallback => "SEM*",
            SemanticAvailability::NotInstalled => "LEX",
            SemanticAvailability::NeedsConsent => "LEX",
            SemanticAvailability::Downloading { .. } => "DL...",
            SemanticAvailability::Verifying => "VFY...",
            SemanticAvailability::IndexBuilding { .. } => "IDX...",
            SemanticAvailability::IndexStale { .. } => "STALE",
            SemanticAvailability::Disabled { .. } => "OFF",
            SemanticAvailability::ModelMissing { .. } => "NOMODEL",
            SemanticAvailability::IndexMissing { .. } => "NOIDX",
            SemanticAvailability::DatabaseUnavailable { .. } => "NODB",
            SemanticAvailability::LoadFailed { .. } => "ERR",
            SemanticAvailability::UpdateAvailable { .. } => "UPD",
        }
    }

    /// Get a detailed summary for display.
    pub fn summary(&self) -> String {
        match self {
            SemanticAvailability::Ready { embedder_id } => {
                format!("semantic ready ({embedder_id})")
            }
            SemanticAvailability::NotInstalled => "model not installed".to_string(),
            SemanticAvailability::NeedsConsent => "consent required for model download".to_string(),
            SemanticAvailability::Downloading {
                progress_pct,
                bytes_downloaded,
                total_bytes,
            } => {
                let mb_done = *bytes_downloaded as f64 / 1_048_576.0;
                let mb_total = *total_bytes as f64 / 1_048_576.0;
                format!("downloading model: {progress_pct}% ({mb_done:.1}/{mb_total:.1} MB)")
            }
            SemanticAvailability::Verifying => "verifying model checksum".to_string(),
            SemanticAvailability::IndexBuilding {
                items_indexed,
                total_items,
                progress_pct,
                ..
            } => {
                if let Some(pct) = progress_pct {
                    format!("building index: {pct}% ({items_indexed}/{total_items})")
                } else {
                    format!("building index: {items_indexed}/{total_items}")
                }
            }
            SemanticAvailability::IndexStale { reason, .. } => {
                format!("semantic assets not current: {reason}")
            }
            SemanticAvailability::HashFallback => "using explicit hash mode".to_string(),
            SemanticAvailability::Disabled { reason } => {
                format!("semantic disabled: {reason}")
            }
            SemanticAvailability::ModelMissing { model_dir, .. } => {
                format!("model missing at {}", model_dir.display())
            }
            SemanticAvailability::IndexMissing { index_path } => {
                format!("vector index missing at {}", index_path.display())
            }
            SemanticAvailability::DatabaseUnavailable { error, .. } => {
                format!("db unavailable ({error})")
            }
            SemanticAvailability::LoadFailed { context } => {
                format!("semantic load failed ({context})")
            }
            SemanticAvailability::UpdateAvailable {
                current_revision,
                latest_revision,
                ..
            } => {
                format!("update available: {current_revision} -> {latest_revision}")
            }
        }
    }
}

pub struct SemanticContext {
    pub embedder: Arc<dyn Embedder>,
    /// These owners may come from the monolithic writer path or the prototype
    /// shard manifest. Exact search consumes the retained owners directly.
    /// Progressive/two-tier serving remains fail-closed for every source,
    /// because its current FrankenSearch constructors reopen paths. Do not
    /// populate these owners by reopening
    /// `ValidatedSemanticGeneration::artifact_paths`: that map is diagnostic
    /// resolution only until the immutable-generation validator can hand
    /// serving code retained FSVI witnesses/owners and FrankenSearch can
    /// consume those owners without reopening.
    pub artifacts: Vec<SemanticIndexArtifact>,
    pub quality_artifact: Option<SemanticIndexArtifact>,
    pub filter_maps: SemanticFilterMaps,
    pub roles: Option<HashSet<u8>>,
}

pub struct SemanticSetup {
    pub availability: SemanticAvailability,
    pub context: Option<SemanticContext>,
}

const SEMANTIC_POINTER_PROBE_FAILED_REASON: &str = "semantic_pointer_probe_failed";

fn selected_generation_owner_requirement(data_dir: &Path) -> Option<SemanticAvailability> {
    match std::fs::symlink_metadata(SemanticCurrentPointerV1::path(data_dir)) {
        Ok(_) => Some(SemanticAvailability::LoadFailed {
            context: SemanticProgressiveUnavailableReason::OwnerBackedReaderRequired
                .code()
                .to_string(),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => Some(SemanticAvailability::LoadFailed {
            context: SEMANTIC_POINTER_PROBE_FAILED_REASON.to_string(),
        }),
    }
}

fn semantic_sidecar_path(data_dir: &Path, recorded_path: &str) -> Option<PathBuf> {
    semantic_shard_artifact_path_is_safe(recorded_path).then(|| data_dir.join(recorded_path))
}

fn validate_vector_index_contract(
    index: &VectorIndex,
    expected_embedder_id: &str,
    expected_dimension: usize,
) -> Result<(), String> {
    let expected_revision =
        expected_vector_space_revision(expected_embedder_id).ok_or_else(|| {
            format!("no current vector-space revision for embedder {expected_embedder_id}")
        })?;
    if index.embedder_id() != expected_embedder_id
        || index.dimension() != expected_dimension
        || index.embedder_revision() != expected_revision
    {
        return Err(format!(
            "incompatible vector index: expected embedder {expected_embedder_id} revision {expected_revision} dimension {expected_dimension}, found embedder {} revision {} dimension {}; rebuild semantic vectors",
            index.embedder_id(),
            index.embedder_revision(),
            index.dimension()
        ));
    }
    Ok(())
}

fn open_validated_semantic_artifact(
    index_path: &Path,
    ann_path: Option<PathBuf>,
    expected_embedder_id: &str,
    expected_dimension: usize,
) -> Result<SemanticIndexArtifact, String> {
    let artifact = SemanticIndexArtifact::open(index_path, ann_path)
        .map_err(|error| format!("vector index: {error}"))?;
    validate_vector_index_contract(artifact.index(), expected_embedder_id, expected_dimension)?;
    Ok(artifact)
}

fn semantic_artifact_candidate_exists(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!(
            "inspect semantic artifact candidate {}: {error}",
            path.display()
        )),
    }
}

fn matching_complete_shard_records(
    data_dir: &Path,
    tier: TierKind,
    embedder_id: &str,
    db_fingerprint: &str,
) -> Result<Option<Vec<SemanticShardRecord>>, String> {
    let manifest = match SemanticShardManifest::load(data_dir) {
        Ok(Some(manifest)) => manifest,
        Ok(None) => return Ok(None),
        Err(err) => return Err(format!("semantic shard manifest: {err}")),
    };
    let summary = manifest.summary(tier, embedder_id, db_fingerprint);
    if !summary.complete {
        return Ok(None);
    }

    let mut records = manifest
        .shards
        .into_iter()
        .filter(|shard| shard.matches_generation(tier, embedder_id, db_fingerprint))
        .collect::<Vec<_>>();
    records.sort_by_key(|shard| shard.shard_index);
    if records.len() != usize::try_from(summary.shard_count).unwrap_or(usize::MAX) {
        return Ok(None);
    }

    let Some(first) = records.first() else {
        return Ok(None);
    };
    for (expected_index, shard) in records.iter().enumerate() {
        if shard.shard_index != u32::try_from(expected_index).unwrap_or(u32::MAX)
            || !shard.ready
            || !shard.mmap_ready
            || shard.model_revision != first.model_revision
            || shard.schema_version != crate::search::policy::SEMANTIC_SCHEMA_VERSION
            || shard.chunking_version != crate::search::policy::CHUNKING_STRATEGY_VERSION
            || shard.dimension == 0
            || shard.dimension != first.dimension
            || shard.total_conversations != first.total_conversations
        {
            return Ok(None);
        }
        let Some(path) = semantic_sidecar_path(data_dir, &shard.index_path) else {
            return Ok(None);
        };
        if !semantic_artifact_candidate_exists(&path)? {
            return Ok(None);
        }
    }

    Ok(Some(records))
}

fn load_complete_shard_artifacts(
    data_dir: &Path,
    embedder_id: &str,
    expected_dimension: usize,
    db_fingerprint: &str,
) -> Result<Option<Vec<SemanticIndexArtifact>>, String> {
    for tier in [TierKind::Quality, TierKind::Fast] {
        let Some(records) =
            matching_complete_shard_records(data_dir, tier, embedder_id, db_fingerprint)?
        else {
            continue;
        };

        let mut artifacts = Vec::with_capacity(records.len());
        for shard in records {
            let Some(path) = semantic_sidecar_path(data_dir, &shard.index_path) else {
                return Ok(None);
            };
            let ann_path = if shard.ann_ready {
                shard
                    .ann_index_path
                    .as_deref()
                    .and_then(|recorded| semantic_sidecar_path(data_dir, recorded))
                    .and_then(|candidate| {
                        let selected = select_existing_ann_candidate(candidate.clone());
                        if selected.is_none() {
                            tracing::warn!(
                                path = %candidate.display(),
                                embedder = embedder_id,
                                shard = shard.shard_index,
                                "semantic shard manifest marks ANN ready but its exact sidecar is missing"
                            );
                        }
                        selected
                    })
            } else {
                None
            };
            let artifact = SemanticIndexArtifact::open(&path, ann_path)
                .map_err(|err| format!("semantic shard vector index {}: {err}", path.display()))?;
            if shard.dimension != expected_dimension {
                return Err(format!(
                    "semantic shard manifest {} declares dimension {}, expected {expected_dimension}",
                    path.display(),
                    shard.dimension
                ));
            }
            validate_vector_index_contract(artifact.index(), embedder_id, expected_dimension)
                .map_err(|error| {
                    format!("semantic shard vector index {}: {error}", path.display())
                })?;
            artifacts.push(artifact);
        }
        if !artifacts.is_empty() {
            tracing::info!(
                tier = tier.as_str(),
                embedder = embedder_id,
                shard_count = artifacts.len(),
                "loaded complete semantic shard generation"
            );
            return Ok(Some(artifacts));
        }
    }

    Ok(None)
}

fn select_existing_ann_candidate(path: PathBuf) -> Option<PathBuf> {
    match std::fs::symlink_metadata(&path) {
        Ok(_) => Some(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => Some(path),
    }
}

fn complete_shard_generation_candidate_exists(data_dir: &Path, embedder_id: &str) -> bool {
    let manifest = match SemanticShardManifest::load(data_dir) {
        Ok(Some(manifest)) => manifest,
        Ok(None) => return false,
        Err(err) => {
            tracing::debug!(
                error = %err,
                embedder = embedder_id,
                "semantic shard candidate probe could not load manifest"
            );
            return false;
        }
    };

    let mut candidates = std::collections::HashSet::new();
    for shard in manifest
        .shards
        .iter()
        .filter(|shard| shard.embedder_id == embedder_id)
    {
        candidates.insert((shard.tier, shard.db_fingerprint.as_str()));
    }

    candidates
        .into_iter()
        .any(|(tier, db_fingerprint)| manifest.summary(tier, embedder_id, db_fingerprint).complete)
}

fn load_complete_shard_artifacts_for_current_db(
    data_dir: &Path,
    db_path: &Path,
    embedder_id: &str,
    expected_dimension: usize,
    context_label: &'static str,
) -> Option<Vec<SemanticIndexArtifact>> {
    let db_fingerprint = match crate::indexer::lexical_storage_fingerprint_for_db(db_path) {
        Ok(fingerprint) => fingerprint,
        Err(err) => {
            tracing::debug!(
                error = %format!("{err:#}"),
                embedder = embedder_id,
                context = context_label,
                "semantic shard context unavailable: failed to fingerprint current DB"
            );
            return None;
        }
    };

    match load_complete_shard_artifacts(data_dir, embedder_id, expected_dimension, &db_fingerprint)
    {
        Ok(artifacts) => artifacts,
        Err(err) => {
            tracing::debug!(
                error = %err,
                embedder = embedder_id,
                context = context_label,
                "semantic shard context unavailable"
            );
            None
        }
    }
}

/// Load semantic context with optional version mismatch checking.
///
/// If `check_for_updates` is true, this function will check if the installed
/// model version matches the manifest and return `UpdateAvailable` if they differ.
pub fn load_semantic_context(data_dir: &Path, db_path: &Path) -> SemanticSetup {
    load_semantic_context_for_embedder(data_dir, db_path, active_policy_embedder_name())
}

/// Load the active policy context without initializing its local model yet.
pub fn load_semantic_context_deferred(data_dir: &Path, db_path: &Path) -> SemanticSetup {
    load_semantic_context_for_embedder_deferred(data_dir, db_path, active_policy_embedder_name())
}

pub fn load_semantic_context_for_embedder(
    data_dir: &Path,
    db_path: &Path,
    embedder_name: &str,
) -> SemanticSetup {
    load_semantic_context_inner(data_dir, db_path, true, embedder_name, false)
}

/// Load index/filter metadata now while deferring the local model itself.
///
/// This is the daemon-first CLI path: the returned embedder preserves the
/// vector index's stable id/dimension contract, but initializes local inference
/// only if the daemon wrapper needs its fallback (#347).
pub fn load_semantic_context_for_embedder_deferred(
    data_dir: &Path,
    db_path: &Path,
    embedder_name: &str,
) -> SemanticSetup {
    load_semantic_context_inner(data_dir, db_path, true, embedder_name, true)
}

/// Probe semantic availability without loading the embedder or DB-backed
/// filter maps. The selected FSVI is opened through the same exact-artifact
/// policy as serving so status cannot advertise an artifact a query rejects.
pub(crate) fn probe_semantic_availability(data_dir: &Path) -> SemanticAvailability {
    probe_semantic_availability_for_embedder(data_dir, active_policy_embedder_name())
}

pub(crate) fn probe_semantic_availability_for_embedder(
    data_dir: &Path,
    embedder_name: &str,
) -> SemanticAvailability {
    if let Some(availability) = selected_generation_owner_requirement(data_dir) {
        return availability;
    }
    let Some(canonical_name) = FastEmbedder::canonical_name(embedder_name) else {
        return SemanticAvailability::LoadFailed {
            context: format!("unsupported semantic embedder: {embedder_name}; supported: minilm"),
        };
    };
    let Some(config) = FastEmbedder::config_for(canonical_name) else {
        return SemanticAvailability::LoadFailed {
            context: format!("unknown semantic embedder: {embedder_name}"),
        };
    };
    let Some(model_dir) = FastEmbedder::runtime_model_dir_for(data_dir, canonical_name) else {
        return SemanticAvailability::LoadFailed {
            context: format!("no model directory mapping for semantic embedder: {embedder_name}"),
        };
    };
    let manifest =
        ModelManifest::for_embedder(canonical_name).unwrap_or_else(ModelManifest::minilm_v2);
    let semantic_policy = SemanticPolicy::resolve(&CliSemanticOverrides::default());
    let acquisition_policy = ModelAcquisitionPolicy::from_semantic_policy(&semantic_policy);
    let cache_report = classify_model_cache_metadata(&model_dir, &manifest, &acquisition_policy);

    if let Some(availability) =
        semantic_availability_from_cache_state(&model_dir, &cache_report.state, true)
    {
        return availability;
    }

    let index_path = vector_index_path(data_dir, &config.embedder_id);
    match semantic_artifact_candidate_exists(&index_path) {
        Ok(true) => {}
        Ok(false) => return SemanticAvailability::IndexMissing { index_path },
        Err(context) => return SemanticAvailability::LoadFailed { context },
    }

    if let Err(context) =
        open_validated_semantic_artifact(&index_path, None, &config.embedder_id, config.dimension)
    {
        return SemanticAvailability::LoadFailed { context };
    }

    SemanticAvailability::Ready {
        embedder_id: config.embedder_id,
    }
}

/// Probe hash semantic availability without opening the DB. The selected FSVI
/// is admitted through the same exact-artifact policy as serving.
pub(crate) fn probe_hash_semantic_availability(data_dir: &Path) -> SemanticAvailability {
    if let Some(availability) = selected_generation_owner_requirement(data_dir) {
        return availability;
    }
    let embedder = HashEmbedder::default();
    let index_path = vector_index_path(data_dir, embedder.id());
    match semantic_artifact_candidate_exists(&index_path) {
        Ok(false) => SemanticAvailability::IndexMissing { index_path },
        Err(context) => SemanticAvailability::LoadFailed { context },
        Ok(true) => match open_validated_semantic_artifact(
            &index_path,
            None,
            embedder.id(),
            embedder.dimension(),
        ) {
            Ok(_) => SemanticAvailability::HashFallback,
            Err(context) => SemanticAvailability::LoadFailed { context },
        },
    }
}

/// Load hash-based semantic context (no model download required).
pub fn load_hash_semantic_context(data_dir: &Path, db_path: &Path) -> SemanticSetup {
    if let Some(availability) = selected_generation_owner_requirement(data_dir) {
        return SemanticSetup {
            availability,
            context: None,
        };
    }
    let embedder = HashEmbedder::default();
    let index_path = vector_index_path(data_dir, embedder.id());
    let monolithic_present = match semantic_artifact_candidate_exists(&index_path) {
        Ok(present) => present,
        Err(context) => {
            return SemanticSetup {
                availability: SemanticAvailability::LoadFailed { context },
                context: None,
            };
        }
    };
    let shard_artifacts = if monolithic_present
        || complete_shard_generation_candidate_exists(data_dir, embedder.id())
    {
        load_complete_shard_artifacts_for_current_db(
            data_dir,
            db_path,
            embedder.id(),
            embedder.dimension(),
            "hash semantic",
        )
    } else {
        None
    };
    if !monolithic_present && shard_artifacts.is_none() {
        return SemanticSetup {
            availability: SemanticAvailability::IndexMissing { index_path },
            context: None,
        };
    }

    let storage = match FrankenStorage::open_readonly(db_path) {
        Ok(storage) => storage,
        Err(err) => {
            return SemanticSetup {
                availability: SemanticAvailability::DatabaseUnavailable {
                    db_path: db_path.to_path_buf(),
                    error: err.to_string(),
                },
                context: None,
            };
        }
    };

    if let Some(availability) = refuse_stale_semantic_assets(
        data_dir,
        &storage,
        embedder.id(),
        &SemanticAvailability::HashFallback,
        crate::search::asset_state::SemanticPreference::HashFallback,
    ) {
        return SemanticSetup {
            availability,
            context: None,
        };
    }

    let filter_maps = match SemanticFilterMaps::from_storage(&storage) {
        Ok(maps) => maps,
        Err(err) => {
            return SemanticSetup {
                availability: SemanticAvailability::LoadFailed {
                    context: format!("filter maps: {err}"),
                },
                context: None,
            };
        }
    };

    let artifacts = if let Some(artifacts) = shard_artifacts {
        artifacts
    } else {
        let ann_path = hnsw_index_path(data_dir, embedder.id());
        let ann_path = select_existing_ann_candidate(ann_path);
        match open_validated_semantic_artifact(
            &index_path,
            ann_path,
            embedder.id(),
            embedder.dimension(),
        ) {
            Ok(artifact) => vec![artifact],
            Err(context) => {
                return SemanticSetup {
                    availability: SemanticAvailability::LoadFailed { context },
                    context: None,
                };
            }
        }
    };

    let roles = Some(HashSet::from([ROLE_USER, ROLE_ASSISTANT]));
    let embedder = Arc::new(embedder) as Arc<dyn Embedder>;

    SemanticSetup {
        availability: SemanticAvailability::HashFallback,
        context: Some(SemanticContext {
            embedder,
            artifacts,
            quality_artifact: None,
            filter_maps,
            roles,
        }),
    }
}

/// Load semantic context without version checking.
///
/// Use this when you've already acknowledged an update and want to load
/// the model anyway.
pub fn load_semantic_context_no_version_check(data_dir: &Path, db_path: &Path) -> SemanticSetup {
    load_semantic_context_inner(
        data_dir,
        db_path,
        false,
        active_policy_embedder_name(),
        false,
    )
}

fn load_semantic_context_inner(
    data_dir: &Path,
    db_path: &Path,
    check_for_updates: bool,
    embedder_name: &str,
    defer_embedder_load: bool,
) -> SemanticSetup {
    if let Some(availability) = selected_generation_owner_requirement(data_dir) {
        return SemanticSetup {
            availability,
            context: None,
        };
    }
    let Some(canonical_name) = FastEmbedder::canonical_name(embedder_name) else {
        return SemanticSetup {
            availability: SemanticAvailability::LoadFailed {
                context: format!(
                    "unsupported semantic embedder: {embedder_name}; supported: minilm"
                ),
            },
            context: None,
        };
    };
    let Some(config) = FastEmbedder::config_for(canonical_name) else {
        return SemanticSetup {
            availability: SemanticAvailability::LoadFailed {
                context: format!("unknown semantic embedder: {embedder_name}"),
            },
            context: None,
        };
    };
    let Some(model_dir) = FastEmbedder::runtime_model_dir_for(data_dir, canonical_name) else {
        return SemanticSetup {
            availability: SemanticAvailability::LoadFailed {
                context: format!(
                    "no model directory mapping for semantic embedder: {embedder_name}"
                ),
            },
            context: None,
        };
    };
    let manifest =
        ModelManifest::for_embedder(canonical_name).unwrap_or_else(ModelManifest::minilm_v2);
    let semantic_policy = SemanticPolicy::resolve(&CliSemanticOverrides::default());
    let acquisition_policy = ModelAcquisitionPolicy::from_semantic_policy(&semantic_policy);
    let cache_report = classify_model_cache(&model_dir, &manifest, &acquisition_policy);

    if let Some(availability) =
        semantic_availability_from_cache_state(&model_dir, &cache_report.state, check_for_updates)
    {
        return SemanticSetup {
            availability,
            context: None,
        };
    }

    let index_path = vector_index_path(data_dir, &config.embedder_id);
    let monolithic_present = match semantic_artifact_candidate_exists(&index_path) {
        Ok(present) => present,
        Err(context) => {
            return SemanticSetup {
                availability: SemanticAvailability::LoadFailed { context },
                context: None,
            };
        }
    };
    let shard_artifacts = if monolithic_present
        || complete_shard_generation_candidate_exists(data_dir, &config.embedder_id)
    {
        load_complete_shard_artifacts_for_current_db(
            data_dir,
            db_path,
            &config.embedder_id,
            config.dimension,
            "semantic",
        )
    } else {
        None
    };
    if !monolithic_present && shard_artifacts.is_none() {
        return SemanticSetup {
            availability: SemanticAvailability::IndexMissing { index_path },
            context: None,
        };
    }

    let storage = match FrankenStorage::open_readonly(db_path) {
        Ok(storage) => storage,
        Err(err) => {
            return SemanticSetup {
                availability: SemanticAvailability::DatabaseUnavailable {
                    db_path: db_path.to_path_buf(),
                    error: err.to_string(),
                },
                context: None,
            };
        }
    };

    if let Some(availability) = refuse_stale_semantic_assets(
        data_dir,
        &storage,
        &config.embedder_id,
        &SemanticAvailability::Ready {
            embedder_id: config.embedder_id.clone(),
        },
        crate::search::asset_state::SemanticPreference::DefaultModel,
    ) {
        return SemanticSetup {
            availability,
            context: None,
        };
    }

    let filter_maps = match SemanticFilterMaps::from_storage(&storage) {
        Ok(maps) => maps,
        Err(err) => {
            return SemanticSetup {
                availability: SemanticAvailability::LoadFailed {
                    context: format!("filter maps: {err}"),
                },
                context: None,
            };
        }
    };

    let artifacts = if let Some(artifacts) = shard_artifacts {
        artifacts
    } else {
        let ann_path = hnsw_index_path(data_dir, &config.embedder_id);
        let ann_path = select_existing_ann_candidate(ann_path);
        match open_validated_semantic_artifact(
            &index_path,
            ann_path,
            &config.embedder_id,
            config.dimension,
        ) {
            Ok(artifact) => vec![artifact],
            Err(context) => {
                return SemanticSetup {
                    availability: SemanticAvailability::LoadFailed { context },
                    context: None,
                };
            }
        }
    };

    let embedder: Arc<dyn Embedder> = match if defer_embedder_load {
        LazyFastEmbedder::new(data_dir, canonical_name)
            .map(|embedder| Arc::new(embedder) as Arc<dyn Embedder>)
    } else {
        FastEmbedder::load_by_name(data_dir, canonical_name)
            .map(|embedder| Arc::new(embedder) as Arc<dyn Embedder>)
    } {
        Ok(embedder) => embedder,
        Err(err) => {
            return SemanticSetup {
                availability: SemanticAvailability::LoadFailed {
                    context: format!("model load: {err}"),
                },
                context: None,
            };
        }
    };

    let roles = Some(HashSet::from([ROLE_USER, ROLE_ASSISTANT]));

    SemanticSetup {
        availability: SemanticAvailability::Ready {
            embedder_id: embedder.id().to_string(),
        },
        context: Some(SemanticContext {
            embedder,
            artifacts,
            quality_artifact: None,
            filter_maps,
            roles,
        }),
    }
}

/// GH #404: refuse vector assets that load but do not describe the current
/// database.
///
/// `cass status` already computes a readiness verdict (`can_search`,
/// `fallback_mode`) from the semantic manifest against the live DB
/// fingerprint, but the query path used to admit any artifact that merely
/// parsed. A stale or mid-backfill FSVI/HNSW has no relevance floor, so an
/// out-of-vocabulary query returned `k` arbitrary neighbours while status
/// said `fallback_mode: "lexical"`. This applies the same verdict before a
/// context is handed to `SearchClient`, reusing the read-only handle the
/// caller already opened for filter maps (no second archive open).
///
/// Returns `Some(unavailable)` when serving must be refused; `None` when the
/// assets are current (or when there is no manifest to contradict them, which
/// preserves legacy manifest-less installs).
fn refuse_stale_semantic_assets(
    data_dir: &Path,
    storage: &FrankenStorage,
    served_embedder_id: &str,
    availability_if_served: &SemanticAvailability,
    preference: crate::search::asset_state::SemanticPreference,
) -> Option<SemanticAvailability> {
    match storage.semantic_identity_rebuild_required() {
        Ok(true) => {
            return Some(SemanticAvailability::IndexStale {
                embedder_id: served_embedder_id.to_string(),
                reason: "canonical agent/source identity changed; run 'cass index --semantic' to rebuild semantic filter metadata"
                    .to_string(),
            });
        }
        Ok(false) => {}
        Err(err) => {
            return Some(SemanticAvailability::LoadFailed {
                context: format!(
                    "checking canonical semantic identity invalidation marker: {err}"
                ),
            });
        }
    }
    let db_fingerprint = match crate::indexer::lexical_storage_fingerprint_for_storage(storage) {
        Ok(fingerprint) => fingerprint,
        Err(err) => {
            // The archive answered the open but not the fingerprint queries;
            // the filter-map load right after this will surface the real
            // error, so do not invent a staleness verdict here.
            tracing::debug!(
                error = %format!("{err:#}"),
                embedder = served_embedder_id,
                "semantic staleness gate skipped: could not fingerprint current DB"
            );
            return None;
        }
    };
    let state = crate::search::asset_state::semantic_state_from_availability(
        data_dir,
        availability_if_served,
        preference,
        Some(&db_fingerprint),
    );
    // The tier that actually backs the artifact being served. The overall
    // verdict can be `can_search` on the strength of the *other* tier (e.g.
    // a current hash fast tier while the MiniLM quality tier is stale), which
    // is fine for status but not for handing out this specific artifact.
    let served_tier = if served_embedder_id == HashEmbedder::default().id() {
        &state.fast_tier
    } else {
        &state.quality_tier
    };
    // Only a record for *this* embedder can vouch for or refuse this artifact;
    // a tier record built by some other embedder says nothing about it.
    let served_tier_describes_artifact =
        served_tier.present && served_tier.embedder_id.as_deref() == Some(served_embedder_id);
    let served_tier_not_current = served_tier_describes_artifact
        && (!served_tier.ready || served_tier.current_db_matches == Some(false));
    if state.can_search && !served_tier_not_current {
        return None;
    }
    let reason = match (&state.hint, served_tier_not_current) {
        (_, true) if state.can_search => format!(
            "published {} tier for {served_embedder_id} does not match the current database; run 'cass index --semantic' to refresh it",
            if served_embedder_id == HashEmbedder::default().id() {
                "fast"
            } else {
                "quality"
            }
        ),
        (Some(hint), _) => format!("{}; {hint}", state.summary),
        (None, _) => state.summary.clone(),
    };
    tracing::info!(
        embedder = served_embedder_id,
        status = state.status,
        fallback_mode = state.fallback_mode,
        "refusing semantic assets that do not describe the current database (GH #404)"
    );
    Some(SemanticAvailability::IndexStale {
        embedder_id: served_embedder_id.to_string(),
        reason,
    })
}

fn active_policy_embedder_name() -> &'static str {
    let semantic_policy = SemanticPolicy::resolve(&CliSemanticOverrides::default());
    FastEmbedder::canonical_name(&semantic_policy.quality_tier_embedder).unwrap_or("unsupported")
}

fn semantic_availability_from_cache_state(
    model_dir: &Path,
    state: &ModelCacheState,
    check_for_updates: bool,
) -> Option<SemanticAvailability> {
    match state {
        ModelCacheState::Acquired { .. }
        | ModelCacheState::PreseededLocal { .. }
        | ModelCacheState::MirrorSourced { .. } => None,
        ModelCacheState::IncompatibleVersion {
            current_revision,
            expected_revision,
        } if check_for_updates => Some(SemanticAvailability::UpdateAvailable {
            embedder_id: FastEmbedder::embedder_id_static().to_string(),
            current_revision: current_revision.clone(),
            latest_revision: expected_revision.clone(),
        }),
        ModelCacheState::IncompatibleVersion { .. } => None,
        ModelCacheState::NotAcquired {
            missing_files,
            needs_consent,
        } => {
            if *needs_consent {
                Some(SemanticAvailability::NeedsConsent)
            } else {
                Some(SemanticAvailability::ModelMissing {
                    model_dir: model_dir.to_path_buf(),
                    missing_files: missing_files.clone(),
                })
            }
        }
        ModelCacheState::Acquiring {
            bytes_present,
            total_bytes,
            ..
        } => {
            let progress_pct = if *total_bytes == 0 {
                0
            } else {
                ((*bytes_present as f64 / *total_bytes as f64) * 100.0).min(100.0) as u8
            };
            Some(SemanticAvailability::Downloading {
                progress_pct,
                bytes_downloaded: *bytes_present,
                total_bytes: *total_bytes,
            })
        }
        ModelCacheState::ChecksumMismatch {
            file,
            expected,
            actual,
        } => Some(SemanticAvailability::LoadFailed {
            context: format!(
                "model checksum mismatch for {file}: expected {expected}, got {actual}"
            ),
        }),
        ModelCacheState::DisabledByPolicy { reason } => Some(SemanticAvailability::Disabled {
            reason: reason.clone(),
        }),
        ModelCacheState::BudgetBlocked {
            required_bytes,
            max_bytes,
        } => Some(SemanticAvailability::Disabled {
            reason: format!(
                "semantic model requires {required_bytes} bytes but policy allows {max_bytes}"
            ),
        }),
        ModelCacheState::QuarantinedCorrupt {
            marker_path,
            reason,
        } => Some(SemanticAvailability::LoadFailed {
            context: format!(
                "model cache quarantined at {}: {reason}",
                marker_path.display()
            ),
        }),
        ModelCacheState::OfflineBlocked { missing_files } => Some(SemanticAvailability::Disabled {
            reason: format!(
                "offline and semantic model is not acquired: missing {}",
                missing_files.join(", ")
            ),
        }),
    }
}

/// Check if the vector index needs rebuilding after a model upgrade.
///
/// This compares the embedder ID, vector-space revision, and dimension in the
/// vector index header with the current native MiniLM contract. Shape equality
/// alone cannot prove compatibility across inference-engine generations.
///
/// Returns `true` if rebuild is needed, `false` otherwise.
pub fn needs_index_rebuild(data_dir: &Path) -> bool {
    let index_path = vector_index_path(data_dir, FastEmbedder::embedder_id_static());

    if !index_path.is_file() {
        // Index doesn't exist, so it needs to be built (not rebuilt)
        return false;
    }

    // Try to load the index and check its embedder ID
    match VectorIndex::open(&index_path) {
        Ok(index) => {
            validate_vector_index_contract(&index, FastEmbedder::embedder_id_static(), 384).is_err()
        }
        Err(_) => {
            // Index is corrupted or unreadable, needs rebuild
            true
        }
    }
}

/// Delete the vector index to force a rebuild.
///
/// Call this after a model upgrade when the user has consented to rebuilding
/// the semantic index. The next index run will rebuild from scratch.
///
/// # Returns
///
/// `Ok(true)` if the index was deleted.
/// `Ok(false)` if the index didn't exist.
/// `Err(_)` if deletion failed.
pub fn delete_vector_index_for_rebuild(data_dir: &Path) -> std::io::Result<bool> {
    let index_path = vector_index_path(data_dir, FastEmbedder::embedder_id_static());

    if index_path.is_file() {
        std::fs::remove_file(&index_path)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Get the model directory path for the default MiniLM model.
pub fn default_model_dir(data_dir: &Path) -> PathBuf {
    FastEmbedder::default_model_dir(data_dir)
}

/// Get the model manifest for the default MiniLM model.
pub fn default_model_manifest() -> ModelManifest {
    ModelManifest::minilm_v2()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    type AvailabilityTuiCase = (
        SemanticAvailability,
        &'static str,
        fn(&SemanticAvailability) -> bool,
    );

    #[test]
    fn test_semantic_availability_ready() {
        let ready = SemanticAvailability::Ready {
            embedder_id: "test-123".into(),
        };
        assert!(ready.summary().contains("semantic ready"));
        assert!(ready.is_ready());
        assert!(!ready.has_update());
        assert!(ready.can_search());
        assert_eq!(ready.status_label(), "SEM");
    }

    #[test]
    fn semantic_sidecar_path_rejects_paths_outside_data_dir() {
        let tmp = tempdir().unwrap();
        let safe = semantic_sidecar_path(tmp.path(), "vector_index/shards/hash/shard-0.fsvi")
            .expect("safe relative shard path");
        assert_eq!(
            safe,
            tmp.path().join("vector_index/shards/hash/shard-0.fsvi")
        );

        for unsafe_path in [
            tmp.path()
                .join("outside.fsvi")
                .to_string_lossy()
                .to_string(),
            "../outside.fsvi".to_string(),
            "vector_index/../outside.fsvi".to_string(),
            "./vector_index/shards/hash/shard-0.fsvi".to_string(),
        ] {
            assert!(
                semantic_sidecar_path(tmp.path(), &unsafe_path).is_none(),
                "unsafe semantic sidecar path should be rejected: {unsafe_path}"
            );
        }
    }

    #[test]
    fn test_semantic_availability_update() {
        let update = SemanticAvailability::UpdateAvailable {
            embedder_id: "test".into(),
            current_revision: "v1".into(),
            latest_revision: "v2".into(),
        };
        assert!(update.summary().contains("update available"));
        assert!(!update.is_ready());
        assert!(update.has_update());
        assert_eq!(update.status_label(), "UPD");
    }

    #[test]
    fn test_semantic_availability_index_building() {
        let building = SemanticAvailability::IndexBuilding {
            embedder_id: "test".into(),
            progress_pct: Some(45),
            items_indexed: 100,
            total_items: 200,
        };
        assert!(building.summary().contains("building index"));
        assert!(building.summary().contains("45%"));
        assert!(building.is_building());
        assert_eq!(building.status_label(), "IDX...");

        let (pct, done, total) = building.index_progress().unwrap();
        assert_eq!(pct, Some(45));
        assert_eq!(done, 100);
        assert_eq!(total, 200);
    }

    #[test]
    fn test_semantic_availability_downloading() {
        let downloading = SemanticAvailability::Downloading {
            progress_pct: 50,
            bytes_downloaded: 10_000_000,
            total_bytes: 20_000_000,
        };
        assert!(downloading.is_downloading());
        assert!(downloading.summary().contains("downloading"));
        assert!(downloading.summary().contains("50%"));
        assert_eq!(downloading.status_label(), "DL...");

        let (pct, bytes, total) = downloading.download_progress().unwrap();
        assert_eq!(pct, 50);
        assert_eq!(bytes, 10_000_000);
        assert_eq!(total, 20_000_000);
    }

    #[test]
    fn test_semantic_availability_tui_states() {
        let cases: &[AvailabilityTuiCase] = &[
            (
                SemanticAvailability::NotInstalled,
                "LEX",
                SemanticAvailability::is_not_installed,
            ),
            (
                SemanticAvailability::NeedsConsent,
                "LEX",
                SemanticAvailability::needs_consent,
            ),
            (SemanticAvailability::Verifying, "VFY...", |state| {
                state.summary().contains("verifying")
            }),
            (SemanticAvailability::HashFallback, "SEM*", |state| {
                state.is_hash_fallback() && state.can_search()
            }),
            (
                SemanticAvailability::Disabled {
                    reason: "offline mode".into(),
                },
                "OFF",
                |state| state.is_disabled() && state.summary().contains("offline"),
            ),
        ];

        for (state, expected_label, predicate) in cases {
            assert_eq!(state.status_label(), *expected_label, "{state:?}");
            assert!(predicate(state), "{state:?}");
        }
    }

    #[test]
    fn test_semantic_availability_error_states() {
        let load_failed = SemanticAvailability::LoadFailed {
            context: "test error".into(),
        };
        assert!(load_failed.is_error());
        assert_eq!(load_failed.status_label(), "ERR");

        let db_unavail = SemanticAvailability::DatabaseUnavailable {
            db_path: PathBuf::from("/test"),
            error: "locked".into(),
        };
        assert!(db_unavail.is_error());
        assert_eq!(db_unavail.status_label(), "NODB");
    }

    #[test]
    fn test_needs_index_rebuild_no_index() {
        let tmp = tempdir().unwrap();
        assert!(!needs_index_rebuild(tmp.path()));
    }

    #[test]
    fn test_delete_vector_index_no_file() {
        let tmp = tempdir().unwrap();
        let result = delete_vector_index_for_rebuild(tmp.path());
        assert!(result.is_ok());
        assert!(!result.unwrap());
    }

    fn write_hash_vector_index(path: &Path, record_count: usize) {
        let embedder = HashEmbedder::default();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create vector index parent");
        }
        let mut writer = VectorIndex::create_with_revision(
            path,
            embedder.id(),
            crate::indexer::semantic::HASH_VECTOR_SPACE_REVISION,
            embedder.dimension(),
            frankensearch::index::Quantization::F16,
        )
        .expect("create hash vector index");
        let mut vector = vec![0.0_f32; embedder.dimension()];
        vector[0] = 1.0;
        for idx in 0..record_count {
            writer
                .write_record(&format!("doc-{idx}"), &vector)
                .expect("write hash vector record");
        }
        writer.finish().expect("finish hash vector index");
    }

    #[test]
    fn exact_artifact_contract_pairs_only_existing_consumer_owned_ann() {
        let tmp = tempdir().expect("hash context fixture");
        let db_path = tmp.path().join("cass.db");
        let storage = FrankenStorage::open(&db_path).expect("create cass db");
        drop(storage);

        let embedder = HashEmbedder::default();
        let fsvi_path = vector_index_path(tmp.path(), embedder.id());
        write_hash_vector_index(&fsvi_path, 1);

        let without_ann = load_hash_semantic_context(tmp.path(), &db_path)
            .context
            .expect("hash context without ANN");
        assert_eq!(without_ann.artifacts.len(), 1);
        assert_eq!(without_ann.artifacts[0].fsvi_path(), fsvi_path);
        assert_eq!(
            without_ann.artifacts[0].ann_path(),
            None,
            "a synthesized-but-missing ANN filename must not become a serving artifact"
        );

        let ann_path = hnsw_index_path(tmp.path(), embedder.id());
        std::fs::write(&ann_path, b"selected CASS ANN sidecar fixture")
            .expect("write selected ANN sidecar");
        let with_ann = load_hash_semantic_context(tmp.path(), &db_path)
            .context
            .expect("hash context with ANN");
        assert_eq!(
            with_ann.artifacts[0].ann_path(),
            Some(ann_path.as_path()),
            "an existing CASS-owned ANN path must stay paired with its exact FSVI"
        );
    }

    /// GH #404: a vector index that parses but whose manifest record was
    /// built against a different database fingerprint must not be served —
    /// `cass status` already reports `can_search:false` for it, and the
    /// query path must agree instead of returning k arbitrary neighbours.
    #[test]
    fn stale_manifest_tier_is_refused_even_when_the_artifact_loads() {
        use crate::search::semantic_manifest::{SemanticManifest, TierKind};

        let tmp = tempdir().expect("stale context fixture");
        let db_path = tmp.path().join("cass.db");
        let storage = FrankenStorage::open(&db_path).expect("create cass db");
        drop(storage);

        let embedder = HashEmbedder::default();
        let fsvi_path = vector_index_path(tmp.path(), embedder.id());
        write_hash_vector_index(&fsvi_path, 1);
        let ann_path = hnsw_index_path(tmp.path(), embedder.id());
        std::fs::write(&ann_path, b"selected CASS ANN sidecar fixture")
            .expect("write selected ANN sidecar");

        // Control: with no manifest at all, the legacy manifest-less artifact
        // still serves (no verdict contradicts it).
        assert!(
            load_hash_semantic_context(tmp.path(), &db_path)
                .context
                .is_some(),
            "manifest-less artifacts keep serving"
        );

        // Canonical identity rewrites (such as Pi -> OMP reclassification)
        // do not change the count/max-id fingerprint. The durable storage
        // marker must therefore override even the legacy manifest-less
        // serving allowance until a semantic republish acknowledges it.
        let storage = FrankenStorage::open(&db_path).expect("open identity marker fixture");
        storage
            .mark_semantic_identity_rebuild_required()
            .expect("mark semantic identity stale");
        drop(storage);
        let identity_stale = load_hash_semantic_context(tmp.path(), &db_path);
        assert!(identity_stale.context.is_none());
        assert!(
            identity_stale.availability.is_index_stale(),
            "identity-invalidated semantic assets must fail closed: {:?}",
            identity_stale.availability
        );
        let storage = FrankenStorage::open(&db_path).expect("reopen identity marker fixture");
        storage
            .complete_semantic_identity_rebuild()
            .expect("complete semantic identity rebuild");
        drop(storage);
        assert!(
            load_hash_semantic_context(tmp.path(), &db_path)
                .context
                .is_some(),
            "an acknowledged semantic republish should restore manifest-less serving"
        );

        // A manifest record whose fingerprint belongs to some other database.
        let mut manifest = SemanticManifest::load_or_default(tmp.path()).expect("manifest");
        assert!(manifest.adopt_legacy_artifact(
            TierKind::Fast,
            embedder.id(),
            "hash",
            embedder.dimension(),
            1,
            1,
            "content-v1:999999:999999:999999",
            "vector_index/index-fnv1a-384.fsvi",
            1,
        ));
        manifest.save(tmp.path()).expect("save stale manifest");

        let setup = load_hash_semantic_context(tmp.path(), &db_path);
        assert!(
            setup.context.is_none(),
            "stale tier must not produce a serving context"
        );
        assert!(
            setup.availability.is_index_stale(),
            "expected IndexStale, got {:?}",
            setup.availability
        );
        assert!(
            !setup.availability.can_search(),
            "stale assets must report can_search=false to the query path"
        );

        // Once the record matches the live database again, serving resumes.
        let current = crate::indexer::lexical_storage_fingerprint_for_db(&db_path)
            .expect("current fingerprint");
        let mut manifest = SemanticManifest::load_or_default(tmp.path()).expect("manifest");
        assert!(manifest.adopt_legacy_artifact(
            TierKind::Fast,
            embedder.id(),
            "hash",
            embedder.dimension(),
            1,
            1,
            &current,
            "vector_index/index-fnv1a-384.fsvi",
            1,
        ));
        manifest.save(tmp.path()).expect("save current manifest");
        assert!(
            load_hash_semantic_context(tmp.path(), &db_path)
                .context
                .is_some(),
            "a current tier record must serve again"
        );
    }

    #[cfg(unix)]
    #[test]
    fn readiness_and_serving_reject_the_same_final_component_fsvi_symlink() {
        use std::os::unix::fs::symlink;

        let tmp = tempdir().expect("FSVI symlink fixture");
        let db_path = tmp.path().join("cass.db");
        let storage = FrankenStorage::open(&db_path).expect("create cass db");
        drop(storage);

        let embedder = HashEmbedder::default();
        let selected_path = vector_index_path(tmp.path(), embedder.id());
        let target_path = selected_path.with_file_name("unselected-target.fsvi");
        write_hash_vector_index(&target_path, 1);
        let target_bytes = std::fs::read(&target_path).expect("read target FSVI before probes");
        symlink(&target_path, &selected_path).expect("create final-component FSVI symlink");

        let serving_error = SemanticIndexArtifact::open(&selected_path, None)
            .expect_err("serving must reject a final-component FSVI symlink");
        assert!(serving_error.to_string().contains("symlink"));

        let readiness = probe_hash_semantic_availability(tmp.path());
        let readiness_context = match readiness {
            SemanticAvailability::LoadFailed { context } => context,
            other => panic!("readiness must reject the same FSVI symlink: {other:?}"),
        };
        let serving = load_hash_semantic_context(tmp.path(), &db_path);
        let serving_context = match serving.availability {
            SemanticAvailability::LoadFailed { context } => context,
            other => panic!("semantic serving must reject the same FSVI symlink: {other:?}"),
        };

        assert_eq!(
            readiness_context, serving_context,
            "readiness and serving must use one exact-artifact admission policy"
        );
        assert!(readiness_context.contains("symlink"));
        assert!(
            serving.context.is_none(),
            "a rejected exact artifact must never produce a serving context"
        );
        assert_eq!(
            std::fs::read_link(&selected_path).expect("read selected FSVI symlink"),
            target_path
        );
        assert_eq!(
            std::fs::read(&target_path).expect("read target FSVI after probes"),
            target_bytes,
            "readiness and serving probes must not mutate the symlink target"
        );

        let broken = tempdir().expect("broken FSVI symlink fixture");
        let broken_db_path = broken.path().join("cass.db");
        let storage = FrankenStorage::open(&broken_db_path).expect("create broken fixture db");
        drop(storage);
        let broken_path = vector_index_path(broken.path(), embedder.id());
        std::fs::create_dir_all(
            broken_path
                .parent()
                .expect("broken selected FSVI must have a parent"),
        )
        .expect("create broken selected FSVI parent");
        symlink(broken.path().join("absent-target.fsvi"), &broken_path)
            .expect("create broken final-component FSVI symlink");

        let broken_readiness = probe_hash_semantic_availability(broken.path());
        let broken_serving = load_hash_semantic_context(broken.path(), &broken_db_path);
        let broken_readiness_context = match broken_readiness {
            SemanticAvailability::LoadFailed { context } => context,
            other => panic!("broken FSVI symlink must be rejected by readiness: {other:?}"),
        };
        let broken_serving_context = match broken_serving.availability {
            SemanticAvailability::LoadFailed { context } => context,
            other => panic!("broken FSVI symlink must be rejected by serving: {other:?}"),
        };
        assert_eq!(broken_readiness_context, broken_serving_context);
        assert!(broken_readiness_context.contains("symlink"));
        assert!(broken_serving.context.is_none());

        let non_file = tempdir().expect("non-file FSVI fixture");
        let non_file_db_path = non_file.path().join("cass.db");
        let storage = FrankenStorage::open(&non_file_db_path).expect("create non-file fixture db");
        drop(storage);
        let non_file_path = vector_index_path(non_file.path(), embedder.id());
        std::fs::create_dir_all(
            non_file_path
                .parent()
                .expect("non-file selected FSVI must have a parent"),
        )
        .expect("create non-file selected FSVI parent");
        std::fs::create_dir(&non_file_path).expect("create directory at selected FSVI path");

        let non_file_readiness = probe_hash_semantic_availability(non_file.path());
        let non_file_serving = load_hash_semantic_context(non_file.path(), &non_file_db_path);
        let non_file_readiness_context = match non_file_readiness {
            SemanticAvailability::LoadFailed { context } => context,
            other => panic!("non-file FSVI must be rejected by readiness: {other:?}"),
        };
        let non_file_serving_context = match non_file_serving.availability {
            SemanticAvailability::LoadFailed { context } => context,
            other => panic!("non-file FSVI must be rejected by serving: {other:?}"),
        };
        assert_eq!(non_file_readiness_context, non_file_serving_context);
        assert!(
            non_file_readiness_context.contains("open semantic FSVI"),
            "non-file selection must reach the exact-artifact opener: {non_file_readiness_context}"
        );
        assert!(non_file_serving.context.is_none());
    }

    #[test]
    fn exact_artifact_contract_ann_selection_preserves_existing_invalid_artifacts() {
        let tmp = tempdir().expect("ANN selection fixture");
        let missing = tmp.path().join("missing.chsw");
        assert_eq!(select_existing_ann_candidate(missing), None);

        let regular = tmp.path().join("regular.chsw");
        std::fs::write(&regular, b"not necessarily valid ANN metadata")
            .expect("write ANN candidate");
        assert_eq!(
            select_existing_ann_candidate(regular.clone()),
            Some(regular)
        );

        let directory = tmp.path().join("directory.chsw");
        std::fs::create_dir(&directory).expect("create invalid ANN directory");
        assert_eq!(
            select_existing_ann_candidate(directory.clone()),
            Some(directory),
            "an existing invalid candidate must reach bounded rejection instead of being mislabeled missing"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let broken_symlink = tmp.path().join("broken-symlink.chsw");
            symlink(tmp.path().join("absent-target"), &broken_symlink)
                .expect("create broken ANN symlink");
            assert_eq!(
                select_existing_ann_candidate(broken_symlink.clone()),
                Some(broken_symlink),
                "a selected final-component symlink must reach explicit rejection"
            );
        }
    }

    #[test]
    fn exact_artifact_contract_pointer_selected_generation_requires_retained_owner() {
        let tmp = tempdir().expect("pointer-selected fixture");
        let db_path = tmp.path().join("cass.db");
        let storage = FrankenStorage::open(&db_path).expect("create cass db");
        drop(storage);

        let embedder = HashEmbedder::default();
        let legacy_path = vector_index_path(tmp.path(), embedder.id());
        write_hash_vector_index(&legacy_path, 1);
        let legacy_bytes = std::fs::read(&legacy_path).expect("read legacy FSVI");
        let legacy_mtime = std::fs::metadata(&legacy_path)
            .and_then(|metadata| metadata.modified())
            .expect("legacy FSVI mtime");

        let pointer = SemanticCurrentPointerV1 {
            schema_version: 1,
            generation_id: "gen-selected-owner-contract".to_string(),
            manifest_sha256: "a".repeat(64),
            selection_epoch: 1,
            selected_at_ms: 1_733_100_000_000,
        };
        let pointer_path = SemanticCurrentPointerV1::path(tmp.path());
        std::fs::create_dir_all(pointer_path.parent().expect("pointer parent"))
            .expect("create pointer parent");
        std::fs::write(
            &pointer_path,
            serde_json::to_vec(&pointer).expect("serialize selected pointer"),
        )
        .expect("write selected pointer");
        let mut entries_before = std::fs::read_dir(tmp.path())
            .expect("list fixture before load")
            .map(|entry| entry.expect("fixture entry").file_name())
            .collect::<Vec<_>>();
        entries_before.sort();

        for availability in [
            probe_hash_semantic_availability(tmp.path()),
            probe_semantic_availability(tmp.path()),
            load_hash_semantic_context(tmp.path(), &db_path).availability,
            load_semantic_context(tmp.path(), &db_path).availability,
        ] {
            assert!(
                matches!(
                    &availability,
                    SemanticAvailability::LoadFailed { context }
                        if context
                            == SemanticProgressiveUnavailableReason::OwnerBackedReaderRequired
                                .code()
                ),
                "pointer-selected generation must fail closed with the stable owner reason: {availability:?}"
            );
        }
        assert!(
            load_hash_semantic_context(tmp.path(), &db_path)
                .context
                .is_none(),
            "a valid legacy file must not bypass the selected-generation owner requirement"
        );
        assert_eq!(
            std::fs::read(&legacy_path).expect("read legacy FSVI after rejected load"),
            legacy_bytes
        );
        assert_eq!(
            std::fs::metadata(&legacy_path)
                .and_then(|metadata| metadata.modified())
                .expect("legacy FSVI mtime after rejected load"),
            legacy_mtime
        );
        let mut entries_after = std::fs::read_dir(tmp.path())
            .expect("list fixture after load")
            .map(|entry| entry.expect("fixture entry").file_name())
            .collect::<Vec<_>>();
        entries_after.sort();
        assert_eq!(
            entries_after, entries_before,
            "fail-closed pointer handling must not create serving aliases"
        );
    }

    #[test]
    fn same_id_and_dimension_with_legacy_revision_requires_rebuild() -> anyhow::Result<()> {
        let tmp = tempdir()?;
        let index_path = vector_index_path(tmp.path(), FastEmbedder::embedder_id_static());
        let Some(parent) = index_path.parent() else {
            anyhow::bail!("vector index path has no parent");
        };
        std::fs::create_dir_all(parent)?;

        let writer = VectorIndex::create_with_revision(
            &index_path,
            FastEmbedder::embedder_id_static(),
            "1.0",
            384,
            frankensearch::index::Quantization::F16,
        )?;
        writer.finish()?;
        assert!(needs_index_rebuild(tmp.path()));

        let expected_revision = expected_vector_space_revision(FastEmbedder::embedder_id_static())
            .ok_or_else(|| anyhow::anyhow!("MiniLM vector-space revision is not registered"))?;
        let writer = VectorIndex::create_with_revision(
            &index_path,
            FastEmbedder::embedder_id_static(),
            expected_revision,
            384,
            frankensearch::index::Quantization::F16,
        )?;
        writer.finish()?;
        assert!(!needs_index_rebuild(tmp.path()));
        Ok(())
    }

    fn semantic_shard_record(
        tier: TierKind,
        embedder_id: &str,
        db_fingerprint: &str,
        shard_index: u32,
        shard_count: u32,
    ) -> SemanticShardRecord {
        SemanticShardRecord {
            tier,
            embedder_id: embedder_id.to_string(),
            model_revision: "test-revision".to_string(),
            schema_version: crate::search::policy::SEMANTIC_SCHEMA_VERSION,
            chunking_version: crate::search::policy::CHUNKING_STRATEGY_VERSION,
            dimension: 384,
            shard_index,
            shard_count,
            doc_count: 1,
            total_conversations: 1,
            db_fingerprint: db_fingerprint.to_string(),
            index_path: format!("vector_index/shards/{embedder_id}/shard-{shard_index}.fsvi"),
            quantization: "f16".to_string(),
            mmap_ready: true,
            ann_index_path: None,
            ann_size_bytes: 0,
            ann_ready: false,
            size_bytes: 128,
            started_at_ms: 1_733_100_000_000,
            completed_at_ms: 1_733_100_000_000 + i64::from(shard_index),
            ready: true,
        }
    }

    #[test]
    fn shard_candidate_probe_is_false_without_manifest() {
        let tmp = tempdir().unwrap();
        assert!(
            !complete_shard_generation_candidate_exists(tmp.path(), "fnv1a-384"),
            "missing shard manifest must not trigger a current-DB fingerprint"
        );
    }

    #[test]
    fn shard_candidate_probe_is_false_for_unreadable_manifest() {
        let tmp = tempdir().unwrap();
        let path = SemanticShardManifest::path(tmp.path());
        std::fs::create_dir_all(path.parent().expect("manifest parent"))
            .expect("create shard manifest dir");
        std::fs::write(&path, b"not json").expect("write invalid shard manifest");

        assert!(
            !complete_shard_generation_candidate_exists(tmp.path(), "fnv1a-384"),
            "corrupt shard metadata must not trigger a query-time current-DB fingerprint"
        );
    }

    #[test]
    fn shard_candidate_probe_ignores_other_or_incomplete_generations() {
        let tmp = tempdir().unwrap();
        let mut manifest = SemanticShardManifest {
            shards: vec![
                semantic_shard_record(TierKind::Fast, "other-384", "fp-other", 0, 1),
                semantic_shard_record(TierKind::Fast, "fnv1a-384", "fp-partial", 0, 2),
            ],
            ..Default::default()
        };
        manifest.save(tmp.path()).expect("save shard manifest");

        assert!(
            !complete_shard_generation_candidate_exists(tmp.path(), "fnv1a-384"),
            "incomplete or unrelated shard generations must not trigger a current-DB fingerprint"
        );
    }

    #[test]
    fn shard_candidate_probe_detects_complete_generation_for_embedder() {
        let tmp = tempdir().unwrap();
        let mut manifest = SemanticShardManifest {
            shards: vec![
                semantic_shard_record(TierKind::Fast, "fnv1a-384", "fp-current", 0, 2),
                semantic_shard_record(TierKind::Fast, "fnv1a-384", "fp-current", 1, 2),
            ],
            ..Default::default()
        };
        manifest.save(tmp.path()).expect("save shard manifest");

        assert!(
            complete_shard_generation_candidate_exists(tmp.path(), "fnv1a-384"),
            "complete candidate generations should allow the current-DB fingerprint check"
        );
    }

    #[test]
    fn load_hash_context_prefers_current_complete_shards_over_monolithic_file() {
        let tmp = tempdir().unwrap();
        let db_path = tmp.path().join("cass.db");
        let storage = FrankenStorage::open(&db_path).expect("create cass db");
        drop(storage);
        let db_fingerprint = crate::indexer::lexical_storage_fingerprint_for_db(&db_path)
            .expect("fingerprint cass db");

        let embedder = HashEmbedder::default();
        write_hash_vector_index(&vector_index_path(tmp.path(), embedder.id()), 1);

        let mut records = Vec::new();
        for shard_index in 0..2_u32 {
            let relative_path = format!("vector_index/shards/hash/shard-{shard_index}.fsvi");
            let shard_path = tmp.path().join(&relative_path);
            write_hash_vector_index(&shard_path, 1);
            records.push(SemanticShardRecord {
                tier: TierKind::Fast,
                embedder_id: embedder.id().to_string(),
                model_revision: "hash".to_string(),
                schema_version: crate::search::policy::SEMANTIC_SCHEMA_VERSION,
                chunking_version: crate::search::policy::CHUNKING_STRATEGY_VERSION,
                dimension: embedder.dimension(),
                shard_index,
                shard_count: 2,
                doc_count: 1,
                total_conversations: 1,
                db_fingerprint: db_fingerprint.clone(),
                index_path: relative_path,
                quantization: "f16".to_string(),
                mmap_ready: true,
                ann_index_path: None,
                ann_size_bytes: 0,
                ann_ready: false,
                size_bytes: std::fs::metadata(&shard_path)
                    .expect("stat hash shard")
                    .len(),
                started_at_ms: 1_733_100_000_000,
                completed_at_ms: 1_733_100_000_000 + i64::from(shard_index),
                ready: true,
            });
        }
        let mut manifest = SemanticShardManifest {
            shards: records,
            ..Default::default()
        };
        manifest.save(tmp.path()).expect("save shard manifest");

        let setup = load_hash_semantic_context(tmp.path(), &db_path);
        assert!(
            matches!(setup.availability, SemanticAvailability::HashFallback),
            "hash semantic availability should remain ready: {:?}",
            setup.availability
        );
        let context = setup
            .context
            .expect("complete current shards should load a semantic context");
        assert_eq!(
            context.artifacts.len(),
            2,
            "complete current shards must not be shadowed by an older monolithic vector file"
        );
        let loaded_records = context
            .artifacts
            .iter()
            .map(|artifact| artifact.index().record_count())
            .sum::<usize>();
        assert_eq!(loaded_records, 2);
    }
}
