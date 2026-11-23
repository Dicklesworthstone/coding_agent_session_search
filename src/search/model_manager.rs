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

mod coverage;
mod external;
mod legacy;

pub use coverage::covered_semantic_record;
pub(crate) use coverage::observe_covered_semantic_artifact;
pub(super) use coverage::observe_covered_semantic_artifact_for_readiness;
#[cfg(all(test, unix))]
pub(super) use coverage::semantic_artifact_admission_count;

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
use crate::search::privacy::PrivacySnapshot;
use crate::search::semantic_manifest::{
    SemanticCurrentPointerV1, SemanticShardManifest, SemanticShardRecord, TierKind,
    semantic_shard_artifact_path_is_safe,
};
use crate::search::vector_index::{
    ROLE_ASSISTANT, ROLE_USER, SemanticFilterMaps, SemanticIndexArtifact,
    SemanticProgressiveUnavailableReason, VectorIndex, vector_index_path,
};
use crate::storage::sqlite::{FrankenStorage, SemanticIdentityTier};

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
        embedder_id: String,
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

/// Bind every loaded owner to the epoch observed before any artifact was
/// opened. The context may wait in a UI worker or caller before attachment;
/// retaining this seal prevents a later setter from renewing an obsolete one.
fn with_semantic_privacy(db_path: &Path, load: impl FnOnce() -> SemanticSetup) -> SemanticSetup {
    let result = (|| -> anyhow::Result<SemanticSetup> {
        let privacy = PrivacySnapshot::for_database(db_path)?;
        let mut setup = load();
        privacy.validate()?;
        if let Some(context) = setup.context.as_mut() {
            context.artifacts = std::mem::take(&mut context.artifacts)
                .into_iter()
                .map(|artifact| artifact.with_privacy_snapshot(&privacy))
                .collect::<anyhow::Result<Vec<_>>>()?;
            context.quality_artifact = context
                .quality_artifact
                .take()
                .map(|artifact| artifact.with_privacy_snapshot(&privacy))
                .transpose()?;
        }
        privacy.validate()?;
        Ok(setup)
    })();
    result.unwrap_or_else(|error| SemanticSetup {
        availability: SemanticAvailability::LoadFailed {
            context: format!("{error:#}"),
        },
        context: None,
    })
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
    manifest: &SemanticShardManifest,
    tier: TierKind,
    embedder_id: &str,
    db_fingerprint: &str,
) -> Result<Option<Vec<SemanticShardRecord>>, String> {
    if !shard_generation_declares_complete(manifest, tier, embedder_id, db_fingerprint) {
        return Ok(None);
    }
    let summary = manifest.summary(tier, embedder_id, db_fingerprint);
    if !summary.complete {
        return Err(format!(
            "selected {} semantic shard generation for {embedder_id} has invalid metadata",
            tier.as_str()
        ));
    }

    let mut records = manifest
        .shards
        .iter()
        .filter(|shard| shard.matches_generation(tier, embedder_id, db_fingerprint))
        .cloned()
        .collect::<Vec<_>>();
    records.sort_by_key(|shard| shard.shard_index);
    if records.len() != usize::try_from(summary.shard_count).unwrap_or(usize::MAX) {
        return Err(
            "selected semantic shard generation has an inconsistent shard count".to_string(),
        );
    }

    let Some(first) = records.first() else {
        return Err("selected semantic shard generation is empty".to_string());
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
            return Err(format!(
                "selected semantic shard {} has incompatible generation metadata",
                shard.shard_index
            ));
        }
        let Some(path) = semantic_sidecar_path(data_dir, &shard.index_path) else {
            return Err(format!(
                "selected semantic shard {} has an unsafe artifact path",
                shard.shard_index
            ));
        };
        if !semantic_artifact_candidate_exists(&path)? {
            return Err(format!(
                "selected semantic shard artifact is missing: {}",
                path.display()
            ));
        }
    }

    Ok(Some(records))
}

fn load_complete_shard_artifacts(
    data_dir: &Path,
    manifest: &SemanticShardManifest,
    embedder_id: &str,
    expected_dimension: usize,
    db_fingerprint: &str,
) -> Result<Option<Vec<SemanticIndexArtifact>>, String> {
    for tier in [TierKind::Quality, TierKind::Fast] {
        let Some(records) =
            matching_complete_shard_records(data_dir, manifest, tier, embedder_id, db_fingerprint)?
        else {
            continue;
        };

        let mut artifacts = Vec::with_capacity(records.len());
        for shard in records {
            let Some(path) = semantic_sidecar_path(data_dir, &shard.index_path) else {
                return Err(format!(
                    "selected semantic shard {} has an unsafe artifact path",
                    shard.shard_index
                ));
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

fn shard_generation_declares_complete(
    manifest: &SemanticShardManifest,
    tier: TierKind,
    embedder_id: &str,
    db_fingerprint: &str,
) -> bool {
    let mut shard_count: Option<u32> = None;
    let mut ready_records = 0_usize;
    for shard in manifest
        .shards
        .iter()
        .filter(|shard| shard.matches_generation(tier, embedder_id, db_fingerprint))
    {
        shard_count =
            Some(shard_count.map_or(shard.shard_count, |count| count.min(shard.shard_count)));
        if shard.ready && shard.mmap_ready {
            ready_records += 1;
        }
    }

    // Selection precedes admission: enough published records to satisfy any
    // declared count require a full validation even when their paths,
    // identities, counts or indices are invalid.
    // A genuinely partial publication may still use the legacy artifact.
    ready_records > 0
        && shard_count
            .is_some_and(|count| ready_records >= usize::try_from(count).unwrap_or(usize::MAX))
}

fn complete_shard_generation_candidate_exists_in_manifest(
    manifest: &SemanticShardManifest,
    embedder_id: &str,
) -> bool {
    let mut candidates = std::collections::HashSet::new();
    for shard in manifest
        .shards
        .iter()
        .filter(|shard| shard.embedder_id == embedder_id)
    {
        candidates.insert((shard.tier, shard.db_fingerprint.as_str()));
    }

    candidates.into_iter().any(|(tier, db_fingerprint)| {
        shard_generation_declares_complete(manifest, tier, embedder_id, db_fingerprint)
    })
}

fn complete_shard_generation_candidate_exists(
    data_dir: &Path,
    embedder_id: &str,
) -> Result<bool, String> {
    let manifest = SemanticShardManifest::load(data_dir)
        .map_err(|err| format!("semantic shard manifest: {err}; rebuild semantic vectors"))?;
    Ok(manifest.is_some_and(|manifest| {
        complete_shard_generation_candidate_exists_in_manifest(&manifest, embedder_id)
    }))
}

#[derive(Debug)]
struct AdmittedSemanticShardGeneration {
    artifacts: Vec<SemanticIndexArtifact>,
    db_fingerprint: String,
}

fn load_complete_shard_artifacts_for_storage(
    data_dir: &Path,
    storage: &FrankenStorage,
    embedder_id: &str,
    expected_dimension: usize,
    context_label: &'static str,
) -> Result<Option<AdmittedSemanticShardGeneration>, String> {
    load_complete_shard_artifacts_with_fingerprint(
        data_dir,
        embedder_id,
        expected_dimension,
        context_label,
        || crate::indexer::lexical_storage_fingerprint_for_storage(storage),
    )
}

fn load_complete_shard_artifacts_with_fingerprint(
    data_dir: &Path,
    embedder_id: &str,
    expected_dimension: usize,
    context_label: &'static str,
    fingerprint: impl FnOnce() -> anyhow::Result<String>,
) -> Result<Option<AdmittedSemanticShardGeneration>, String> {
    // GH #452: a monolithic FSVI is not evidence that shards exist. Probe
    // metadata before computing a possible shard generation's fingerprint;
    // legacy callers already hold the canonical authority used for admission.
    let Some(manifest) = SemanticShardManifest::load(data_dir)
        .map_err(|err| format!("semantic shard manifest: {err}; rebuild semantic vectors"))?
    else {
        return Ok(None);
    };
    if !complete_shard_generation_candidate_exists_in_manifest(&manifest, embedder_id) {
        return Ok(None);
    }
    let db_fingerprint = fingerprint().map_err(|err| {
        format!("{context_label} shard admission could not fingerprint the current DB: {err:#}")
    })?;

    // Retain this manifest snapshot across fingerprinting and admission. A
    // selected cohort that fails validation must never become legacy fallback.
    load_complete_shard_artifacts(
        data_dir,
        &manifest,
        embedder_id,
        expected_dimension,
        &db_fingerprint,
    )
    .map(|artifacts| {
        artifacts.map(|artifacts| AdmittedSemanticShardGeneration {
            artifacts,
            db_fingerprint,
        })
    })
    .map_err(|err| {
        format!("{context_label} shard admission failed: {err}; rebuild semantic vectors")
    })
}

/// Load semantic context with optional version mismatch checking.
///
/// If `check_for_updates` is true, this function will check if the installed
/// model version matches the manifest and return `UpdateAvailable` if they differ.
pub fn load_semantic_context(data_dir: &Path, db_path: &Path) -> SemanticSetup {
    load_semantic_context_for_embedder(data_dir, db_path, active_policy_embedder_name())
}

/// Strict read-only counterpart used by `search --no-maintenance`.
pub fn load_semantic_context_strict(data_dir: &Path, db_path: &Path) -> SemanticSetup {
    load_semantic_context_inner(
        data_dir,
        db_path,
        true,
        active_policy_embedder_name(),
        false,
        true,
    )
}

/// Load the active policy context without initializing its local model yet.
pub fn load_semantic_context_deferred(data_dir: &Path, db_path: &Path) -> SemanticSetup {
    load_semantic_context_for_embedder_deferred(data_dir, db_path, active_policy_embedder_name())
}

/// Strict read-only daemon-first counterpart used by
/// `search --no-maintenance`.
pub fn load_semantic_context_deferred_strict(data_dir: &Path, db_path: &Path) -> SemanticSetup {
    load_semantic_context_inner(
        data_dir,
        db_path,
        true,
        active_policy_embedder_name(),
        true,
        true,
    )
}

pub fn load_semantic_context_for_embedder(
    data_dir: &Path,
    db_path: &Path,
    embedder_name: &str,
) -> SemanticSetup {
    load_semantic_context_inner(data_dir, db_path, true, embedder_name, false, false)
}

pub fn load_semantic_context_for_embedder_strict(
    data_dir: &Path,
    db_path: &Path,
    embedder_name: &str,
) -> SemanticSetup {
    load_semantic_context_inner(data_dir, db_path, true, embedder_name, false, true)
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
    load_semantic_context_inner(data_dir, db_path, true, embedder_name, true, false)
}

pub fn load_semantic_context_for_embedder_deferred_strict(
    data_dir: &Path,
    db_path: &Path,
    embedder_name: &str,
) -> SemanticSetup {
    load_semantic_context_inner(data_dir, db_path, true, embedder_name, true, true)
}

/// Probe semantic availability without loading the embedder or DB-backed
/// filter maps. The selected FSVI is opened through the same exact-artifact
/// policy as serving so status cannot advertise an artifact a query rejects.
#[cfg(test)]
pub(crate) fn probe_semantic_availability(data_dir: &Path) -> SemanticAvailability {
    probe_semantic_availability_for_embedder(data_dir, active_policy_probe_embedder_name())
}

#[cfg(test)]
pub(crate) fn probe_semantic_availability_for_embedder(
    data_dir: &Path,
    embedder_name: &str,
) -> SemanticAvailability {
    probe_semantic_availability_for_embedder_with_coverage_probe(
        data_dir,
        embedder_name,
        &mut coverage::probe,
    )
}

/// Readiness supplies canonical coverage admission here so a certified local
/// artifact is authenticated once. Model-cache gating still runs first; the
/// external backend keeps its existing independent admission path.
pub(super) fn probe_semantic_availability_with_coverage_probe(
    data_dir: &Path,
    coverage_probe: &mut impl FnMut(&Path, &str, usize) -> Result<bool, String>,
) -> SemanticAvailability {
    probe_semantic_availability_for_embedder_with_coverage_probe(
        data_dir,
        active_policy_probe_embedder_name(),
        coverage_probe,
    )
}

fn probe_semantic_availability_for_embedder_with_coverage_probe(
    data_dir: &Path,
    embedder_name: &str,
    coverage_probe: &mut impl FnMut(&Path, &str, usize) -> Result<bool, String>,
) -> SemanticAvailability {
    if embedder_name
        .trim()
        .eq_ignore_ascii_case(crate::search::external_embedder::EXTERNAL_EMBEDDER)
    {
        return external::probe(data_dir);
    }
    if let Some(availability) = selected_generation_owner_requirement(data_dir) {
        return availability;
    }
    let Some(canonical_name) = FastEmbedder::canonical_name(embedder_name) else {
        return SemanticAvailability::LoadFailed {
            context: format!(
                "unsupported semantic embedder: {embedder_name}; supported: minilm, multilingual-minilm"
            ),
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

    if let Some(availability) = semantic_availability_from_cache_state(
        &config.embedder_id,
        &model_dir,
        &cache_report.state,
        true,
    ) {
        return availability;
    }

    match coverage_probe(data_dir, &config.embedder_id, config.dimension) {
        Ok(true) => {
            return SemanticAvailability::Ready {
                embedder_id: config.embedder_id,
            };
        }
        Ok(false) => {}
        Err(context) => return SemanticAvailability::LoadFailed { context },
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
#[cfg(test)]
pub(crate) fn probe_hash_semantic_availability(data_dir: &Path) -> SemanticAvailability {
    probe_hash_semantic_availability_with_coverage_probe(data_dir, &mut coverage::probe)
}

pub(super) fn probe_hash_semantic_availability_with_coverage_probe(
    data_dir: &Path,
    coverage_probe: &mut impl FnMut(&Path, &str, usize) -> Result<bool, String>,
) -> SemanticAvailability {
    if let Some(availability) = selected_generation_owner_requirement(data_dir) {
        return availability;
    }
    let embedder = HashEmbedder::default();
    match coverage_probe(data_dir, embedder.id(), embedder.dimension()) {
        Ok(true) => return SemanticAvailability::HashFallback,
        Ok(false) => {}
        Err(context) => return SemanticAvailability::LoadFailed { context },
    }
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
    load_hash_semantic_context_inner(data_dir, db_path, false)
}

pub fn load_hash_semantic_context_strict(data_dir: &Path, db_path: &Path) -> SemanticSetup {
    load_hash_semantic_context_inner(data_dir, db_path, true)
}

fn load_hash_semantic_context_inner(
    data_dir: &Path,
    db_path: &Path,
    strict_read_only: bool,
) -> SemanticSetup {
    with_semantic_privacy(db_path, || {
        load_hash_semantic_context_at_epoch(data_dir, db_path, strict_read_only)
    })
}

fn load_hash_semantic_context_at_epoch(
    data_dir: &Path,
    db_path: &Path,
    strict_read_only: bool,
) -> SemanticSetup {
    if let Some(availability) = selected_generation_owner_requirement(data_dir) {
        return SemanticSetup {
            availability,
            context: None,
        };
    }
    let embedder = HashEmbedder::default();
    match coverage::load_context(
        data_dir,
        db_path,
        embedder.id(),
        embedder.dimension(),
        strict_read_only,
        || Ok(Arc::new(HashEmbedder::default()) as Arc<dyn Embedder>),
    ) {
        Ok(Some(context)) => {
            return SemanticSetup {
                availability: SemanticAvailability::HashFallback,
                context: Some(context),
            };
        }
        Ok(None) => {}
        Err(context) => {
            return SemanticSetup {
                availability: SemanticAvailability::LoadFailed { context },
                context: None,
            };
        }
    }
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
    if !monolithic_present {
        match complete_shard_generation_candidate_exists(data_dir, embedder.id()) {
            Ok(true) => {}
            Ok(false) => {
                return SemanticSetup {
                    availability: SemanticAvailability::IndexMissing { index_path },
                    context: None,
                };
            }
            Err(context) => {
                return SemanticSetup {
                    availability: SemanticAvailability::LoadFailed { context },
                    context: None,
                };
            }
        }
    }

    let storage = match if strict_read_only {
        FrankenStorage::open_strict_readonly(db_path)
    } else {
        FrankenStorage::open_readonly(db_path)
    } {
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

    let legacy_authority = match legacy::capture(&storage) {
        Ok(authority) => authority,
        Err(context) => {
            return SemanticSetup {
                availability: SemanticAvailability::LoadFailed { context },
                context: None,
            };
        }
    };
    let shard_artifacts = match load_complete_shard_artifacts_for_storage(
        data_dir,
        &storage,
        embedder.id(),
        embedder.dimension(),
        "hash semantic",
    ) {
        Ok(artifacts) => artifacts,
        Err(context) => {
            return SemanticSetup {
                availability: SemanticAvailability::LoadFailed { context },
                context: None,
            };
        }
    };
    if !monolithic_present && shard_artifacts.is_none() {
        return SemanticSetup {
            availability: SemanticAvailability::IndexMissing { index_path },
            context: None,
        };
    }

    if let Some(availability) = refuse_stale_semantic_assets(
        data_dir,
        &storage,
        embedder.id(),
        &SemanticAvailability::HashFallback,
        crate::search::asset_state::SemanticPreference::HashFallback,
        shard_artifacts
            .as_ref()
            .map(|generation| generation.db_fingerprint.as_str()),
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

    let artifacts = if let Some(generation) = shard_artifacts {
        generation.artifacts
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

    let artifacts = match legacy::seal(&storage, &legacy_authority, artifacts) {
        Ok(artifacts) => artifacts,
        Err(context) => {
            return SemanticSetup {
                availability: SemanticAvailability::LoadFailed { context },
                context: None,
            };
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
        false,
    )
}

fn load_semantic_context_inner(
    data_dir: &Path,
    db_path: &Path,
    check_for_updates: bool,
    embedder_name: &str,
    defer_embedder_load: bool,
    strict_read_only: bool,
) -> SemanticSetup {
    with_semantic_privacy(db_path, || {
        load_semantic_context_at_epoch(
            data_dir,
            db_path,
            check_for_updates,
            embedder_name,
            defer_embedder_load,
            strict_read_only,
        )
    })
}

fn load_semantic_context_at_epoch(
    data_dir: &Path,
    db_path: &Path,
    check_for_updates: bool,
    embedder_name: &str,
    defer_embedder_load: bool,
    strict_read_only: bool,
) -> SemanticSetup {
    if embedder_name
        .trim()
        .eq_ignore_ascii_case(crate::search::external_embedder::EXTERNAL_EMBEDDER)
    {
        // The local deferred/daemon fallback is not an external provider. This
        // path admits the external artifact first, then runs its own preflight.
        return external::load(data_dir, db_path, strict_read_only);
    }
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
                    "unsupported semantic embedder: {embedder_name}; supported: minilm, multilingual-minilm"
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

    if let Some(availability) = semantic_availability_from_cache_state(
        &config.embedder_id,
        &model_dir,
        &cache_report.state,
        check_for_updates,
    ) {
        return SemanticSetup {
            availability,
            context: None,
        };
    }

    match coverage::load_context(
        data_dir,
        db_path,
        &config.embedder_id,
        config.dimension,
        strict_read_only,
        || {
            if defer_embedder_load {
                LazyFastEmbedder::new(data_dir, canonical_name)
                    .map(|embedder| Arc::new(embedder) as Arc<dyn Embedder>)
            } else {
                FastEmbedder::load_by_name(data_dir, canonical_name)
                    .map(|embedder| Arc::new(embedder) as Arc<dyn Embedder>)
            }
            .map_err(|error| format!("model load: {error}"))
        },
    ) {
        Ok(Some(context)) => {
            return SemanticSetup {
                availability: SemanticAvailability::Ready {
                    embedder_id: config.embedder_id,
                },
                context: Some(context),
            };
        }
        Ok(None) => {}
        Err(context) => {
            return SemanticSetup {
                availability: SemanticAvailability::LoadFailed { context },
                context: None,
            };
        }
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
    if !monolithic_present {
        match complete_shard_generation_candidate_exists(data_dir, &config.embedder_id) {
            Ok(true) => {}
            Ok(false) => {
                return SemanticSetup {
                    availability: SemanticAvailability::IndexMissing { index_path },
                    context: None,
                };
            }
            Err(context) => {
                return SemanticSetup {
                    availability: SemanticAvailability::LoadFailed { context },
                    context: None,
                };
            }
        }
    }

    let storage = match if strict_read_only {
        FrankenStorage::open_strict_readonly(db_path)
    } else {
        FrankenStorage::open_readonly(db_path)
    } {
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

    let legacy_authority = match legacy::capture(&storage) {
        Ok(authority) => authority,
        Err(context) => {
            return SemanticSetup {
                availability: SemanticAvailability::LoadFailed { context },
                context: None,
            };
        }
    };
    let shard_artifacts = match load_complete_shard_artifacts_for_storage(
        data_dir,
        &storage,
        &config.embedder_id,
        config.dimension,
        "semantic",
    ) {
        Ok(artifacts) => artifacts,
        Err(context) => {
            return SemanticSetup {
                availability: SemanticAvailability::LoadFailed { context },
                context: None,
            };
        }
    };
    if !monolithic_present && shard_artifacts.is_none() {
        return SemanticSetup {
            availability: SemanticAvailability::IndexMissing { index_path },
            context: None,
        };
    }

    if let Some(availability) = refuse_stale_semantic_assets(
        data_dir,
        &storage,
        &config.embedder_id,
        &SemanticAvailability::Ready {
            embedder_id: config.embedder_id.clone(),
        },
        crate::search::asset_state::SemanticPreference::DefaultModel,
        shard_artifacts
            .as_ref()
            .map(|generation| generation.db_fingerprint.as_str()),
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

    let artifacts = if let Some(generation) = shard_artifacts {
        generation.artifacts
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

    let artifacts = match legacy::seal(&storage, &legacy_authority, artifacts) {
        Ok(artifacts) => artifacts,
        Err(context) => {
            return SemanticSetup {
                availability: SemanticAvailability::LoadFailed { context },
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
/// Admitted shards carry the fingerprint of their selected cohort; a later
/// shard publication must not vouch for different artifacts already selected
/// by this loader, including a legacy monolithic fallback.
fn refuse_stale_semantic_assets(
    data_dir: &Path,
    storage: &FrankenStorage,
    served_embedder_id: &str,
    availability_if_served: &SemanticAvailability,
    preference: crate::search::asset_state::SemanticPreference,
    admitted_shard_fingerprint: Option<&str>,
) -> Option<SemanticAvailability> {
    let semantic_identity_tier = if served_embedder_id == HashEmbedder::default().id() {
        SemanticIdentityTier::Fast
    } else {
        SemanticIdentityTier::Quality
    };
    match storage.semantic_identity_rebuild_required(semantic_identity_tier) {
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
                context: format!("checking canonical semantic identity invalidation marker: {err}"),
            });
        }
    }
    let db_fingerprint = match crate::indexer::lexical_storage_fingerprint_for_storage(storage) {
        Ok(fingerprint) => fingerprint,
        Err(err) => {
            return Some(SemanticAvailability::LoadFailed {
                context: format!(
                    "semantic freshness check could not fingerprint the current DB: {err:#}"
                ),
            });
        }
    };
    if let Some(admitted_fingerprint) = admitted_shard_fingerprint {
        return (admitted_fingerprint != db_fingerprint).then(|| SemanticAvailability::IndexStale {
            embedder_id: served_embedder_id.to_string(),
            reason: "admitted semantic shard generation no longer matches the current database; run 'cass index --semantic' to refresh it"
                .to_string(),
        });
    }
    let state = crate::search::asset_state::semantic_state_for_monolithic_serving(
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

/// Readiness probes name the external backend when the policy selects it
/// (`CASS_SEMANTIC_EMBEDDER=external`). Mapping it through the native-model
/// aliases turned it into "unsupported", so status reported "unsupported
/// semantic embedder: unsupported" instead of the external backend's own
/// admission result, such as missing consent (GH #561). The external probe
/// sends no HTTP.
fn active_policy_probe_embedder_name() -> &'static str {
    let semantic_policy = SemanticPolicy::resolve(&CliSemanticOverrides::default());
    probe_embedder_name_for_policy(&semantic_policy.quality_tier_embedder)
}

fn probe_embedder_name_for_policy(quality_tier_embedder: &str) -> &'static str {
    if crate::search::embedder_registry::selects_external(Some(quality_tier_embedder)) {
        crate::search::external_embedder::EXTERNAL_EMBEDDER
    } else {
        FastEmbedder::canonical_name(quality_tier_embedder).unwrap_or("unsupported")
    }
}

fn semantic_availability_from_cache_state(
    embedder_id: &str,
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
            embedder_id: embedder_id.to_string(),
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
                    embedder_id: embedder_id.to_string(),
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

    /// GH #561: a policy that selects the external backend must probe that
    /// backend, not the native-model alias table, so status reports the
    /// external admission result instead of "unsupported semantic embedder:
    /// unsupported".
    #[test]
    fn gh561_policy_selected_external_probe_reaches_external_admission() {
        use crate::search::external_embedder::EXTERNAL_EMBEDDER;
        for selected in ["external", " External "] {
            assert_eq!(probe_embedder_name_for_policy(selected), EXTERNAL_EMBEDDER);
        }
        assert_eq!(probe_embedder_name_for_policy("fastembed"), "minilm");
        assert_eq!(
            probe_embedder_name_for_policy("multilingual"),
            "multilingual-minilm"
        );
        assert_eq!(probe_embedder_name_for_policy("not-a-model"), "unsupported");

        // Without consent the external probe refuses with its own reason; with
        // it, an empty data dir has no publication. Neither is the alias error.
        let tmp = tempdir().unwrap();
        let availability = probe_semantic_availability_for_embedder(
            tmp.path(),
            probe_embedder_name_for_policy("external"),
        );
        assert!(
            !matches!(
                &availability,
                SemanticAvailability::LoadFailed { context }
                    if context.contains("unsupported semantic embedder")
            ),
            "external selection must reach external admission: {availability:?}"
        );
        assert!(!availability.can_search(), "{availability:?}");
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
    fn cache_state_preserves_selected_embedder_identity() {
        let model_dir = Path::new("/tmp/cass/models/multilingual-minilm");
        let missing = semantic_availability_from_cache_state(
            "multilingual-minilm-384",
            model_dir,
            &ModelCacheState::NotAcquired {
                missing_files: vec!["model.safetensors".to_string()],
                needs_consent: false,
            },
            true,
        )
        .expect("missing cache state");
        assert!(matches!(
            missing,
            SemanticAvailability::ModelMissing { embedder_id, model_dir: actual_dir, .. }
                if embedder_id == "multilingual-minilm-384" && actual_dir == model_dir
        ));

        let update = semantic_availability_from_cache_state(
            "multilingual-minilm-384",
            model_dir,
            &ModelCacheState::IncompatibleVersion {
                current_revision: "old".to_string(),
                expected_revision: "new".to_string(),
            },
            true,
        )
        .expect("update cache state");
        assert!(matches!(
            update,
            SemanticAvailability::UpdateAvailable { embedder_id, .. }
                if embedder_id == "multilingual-minilm-384"
        ));
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
            .mark_semantic_identity_rebuild_required(SemanticIdentityTier::Fast)
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
            .complete_semantic_identity_rebuild(SemanticIdentityTier::Fast)
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

        for legacy in [
            "1.0",
            "native-minilm-v1:c9745ed1d9f207416be6d2e6f8de32d1f16199bf",
        ] {
            let writer = VectorIndex::create_with_revision(
                &index_path,
                FastEmbedder::embedder_id_static(),
                legacy,
                384,
                frankensearch::index::Quantization::F16,
            )?;
            writer.finish()?;
            assert!(needs_index_rebuild(tmp.path()));
        }

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

    fn publish_hash_shard_generation(
        data_dir: &Path,
        db_fingerprint: &str,
        relative_path: &str,
    ) -> PathBuf {
        let embedder = HashEmbedder::default();
        let shard_path = data_dir.join(relative_path);
        write_hash_vector_index(&shard_path, 1);
        let mut record = semantic_shard_record(TierKind::Fast, embedder.id(), db_fingerprint, 0, 1);
        record.dimension = embedder.dimension();
        record.model_revision = crate::indexer::semantic::HASH_VECTOR_SPACE_REVISION.to_string();
        record.index_path = relative_path.to_string();
        record.size_bytes = std::fs::metadata(&shard_path)
            .expect("stat hash shard")
            .len();
        SemanticShardManifest {
            shards: vec![record],
            ..Default::default()
        }
        .save(data_dir)
        .expect("publish complete hash shard generation");
        shard_path
    }

    #[test]
    fn shard_candidate_probe_is_false_without_manifest() {
        let tmp = tempdir().unwrap();
        assert!(
            !complete_shard_generation_candidate_exists(tmp.path(), "fnv1a-384")
                .expect("probe missing manifest"),
            "missing shard manifest must not trigger a current-DB fingerprint"
        );
        assert!(
            load_complete_shard_artifacts_with_fingerprint(
                tmp.path(),
                "fnv1a-384",
                384,
                "test",
                || panic!("missing shards must not open the archive for fingerprinting"),
            )
            .expect("missing shards permit legacy fallback")
            .is_none()
        );
    }

    #[test]
    fn shard_candidate_probe_refuses_unreadable_manifest_without_fingerprinting() {
        let tmp = tempdir().unwrap();
        let path = SemanticShardManifest::path(tmp.path());
        std::fs::create_dir_all(path.parent().expect("manifest parent"))
            .expect("create shard manifest dir");
        std::fs::write(&path, b"not json").expect("write invalid shard manifest");

        assert!(
            complete_shard_generation_candidate_exists(tmp.path(), "fnv1a-384").is_err(),
            "corrupt shard metadata must not trigger a query-time current-DB fingerprint"
        );
        assert!(
            load_complete_shard_artifacts_with_fingerprint(
                tmp.path(),
                "fnv1a-384",
                384,
                "test",
                || panic!("invalid shards must not open the archive for fingerprinting"),
            )
            .is_err(),
            "unreadable metadata must not authorize legacy fallback"
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
            !complete_shard_generation_candidate_exists(tmp.path(), "fnv1a-384")
                .expect("probe partial and unrelated generations"),
            "incomplete or unrelated shard generations must not trigger a current-DB fingerprint"
        );
        assert!(
            load_complete_shard_artifacts_with_fingerprint(
                tmp.path(),
                "fnv1a-384",
                384,
                "test",
                || panic!("incomplete or unrelated shards must not fingerprint the archive"),
            )
            .expect("incomplete or unrelated shards permit legacy fallback")
            .is_none()
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
            complete_shard_generation_candidate_exists(tmp.path(), "fnv1a-384")
                .expect("probe complete generation"),
            "complete candidate generations should allow the current-DB fingerprint check"
        );
        let fingerprint_calls = std::cell::Cell::new(0);
        assert!(
            load_complete_shard_artifacts_with_fingerprint(
                tmp.path(),
                "fnv1a-384",
                384,
                "test",
                || {
                    fingerprint_calls.set(fingerprint_calls.get() + 1);
                    Ok("fp-current".to_string())
                },
            )
            .is_err(),
            "metadata completeness alone must not admit missing shard artifacts"
        );
        assert_eq!(
            fingerprint_calls.get(),
            1,
            "a complete candidate must still fingerprint the current archive exactly once"
        );
    }

    #[test]
    fn shard_candidate_fingerprint_failure_refuses_legacy_fallback() {
        let tmp = tempdir().unwrap();
        let mut manifest = SemanticShardManifest {
            shards: vec![semantic_shard_record(
                TierKind::Fast,
                "fnv1a-384",
                "fp-current",
                0,
                1,
            )],
            ..Default::default()
        };
        manifest.save(tmp.path()).expect("save shard manifest");

        let error = load_complete_shard_artifacts_with_fingerprint(
            tmp.path(),
            "fnv1a-384",
            384,
            "test",
            || anyhow::bail!("canonical archive unavailable"),
        )
        .expect_err("an unverifiable shard generation must not authorize legacy fallback");
        assert!(
            error.contains("could not fingerprint the current DB"),
            "{error}"
        );
        assert!(error.contains("canonical archive unavailable"), "{error}");
    }

    #[test]
    fn shard_admission_retains_the_selected_manifest_snapshot() {
        let tmp = tempdir().unwrap();
        let mut manifest = SemanticShardManifest {
            shards: vec![semantic_shard_record(
                TierKind::Fast,
                "fnv1a-384",
                "fp-current",
                0,
                1,
            )],
            ..Default::default()
        };
        manifest
            .save(tmp.path())
            .expect("save selected shard manifest");

        let error = load_complete_shard_artifacts_with_fingerprint(
            tmp.path(),
            "fnv1a-384",
            384,
            "test",
            || {
                SemanticShardManifest::default()
                    .save(tmp.path())
                    .expect("replace on-disk manifest during fingerprinting");
                Ok("fp-current".to_string())
            },
        )
        .expect_err("a manifest replacement must not erase a selected cohort's failed admission");
        assert!(
            error.contains("selected semantic shard artifact is missing"),
            "{error}"
        );
    }

    #[test]
    fn later_shard_publication_cannot_authorize_a_selected_stale_monolith() {
        use crate::search::asset_state::{SemanticPreference, semantic_state_from_availability};
        use crate::search::semantic_manifest::SemanticManifest;

        let tmp = tempdir().expect("publication race fixture");
        let db_path = tmp.path().join("cass.db");
        let storage = FrankenStorage::open(&db_path).expect("create cass db");
        let authority = legacy::capture(&storage).expect("capture canonical authority");
        let current = crate::indexer::lexical_storage_fingerprint_for_storage(&storage)
            .expect("fingerprint current database");
        let embedder = HashEmbedder::default();
        let monolithic_path = vector_index_path(tmp.path(), embedder.id());
        write_hash_vector_index(&monolithic_path, 1);
        let mut manifest = SemanticManifest::load_or_default(tmp.path()).expect("manifest");
        assert!(manifest.adopt_legacy_artifact(
            TierKind::Fast,
            embedder.id(),
            "hash",
            embedder.dimension(),
            1,
            1,
            "older-archive",
            "vector_index/index-fnv1a-384.fsvi",
            1,
        ));
        manifest
            .save(tmp.path())
            .expect("publish stale monolith metadata");

        // Follow the loader's real phases, publishing only after it selected
        // legacy fallback. No thread scheduling or canonical DB change is needed.
        let selected_shards = load_complete_shard_artifacts_for_storage(
            tmp.path(),
            &storage,
            embedder.id(),
            embedder.dimension(),
            "test",
        )
        .expect("select artifacts before publication");
        assert!(selected_shards.is_none());
        let shard_path = publish_hash_shard_generation(
            tmp.path(),
            &current,
            "vector_index/shards/hash/published-after-selection.fsvi",
        );
        legacy::validate(&storage, &authority)
            .expect("sidecar publication leaves canonical authority unchanged");

        let status = semantic_state_from_availability(
            tmp.path(),
            &SemanticAvailability::HashFallback,
            SemanticPreference::HashFallback,
            Some(&current),
        );
        assert!(
            status.can_search,
            "status sees the new current shard cohort"
        );
        assert_eq!(
            status.vector_index_path.as_deref(),
            Some(shard_path.as_path())
        );
        let refusal = refuse_stale_semantic_assets(
            tmp.path(),
            &storage,
            embedder.id(),
            &SemanticAvailability::HashFallback,
            SemanticPreference::HashFallback,
            selected_shards
                .as_ref()
                .map(|generation| generation.db_fingerprint.as_str()),
        )
        .expect("new shard readiness must not authorize the selected old monolith");
        assert!(refusal.is_index_stale(), "{refusal:?}");
        assert_eq!(
            open_validated_semantic_artifact(
                &monolithic_path,
                None,
                embedder.id(),
                embedder.dimension(),
            )
            .expect("the stale monolith is readable, so admission must reject its freshness")
            .fsvi_path(),
            monolithic_path
        );

        drop(storage);
        let setup = load_hash_semantic_context(tmp.path(), &db_path);
        let context = setup
            .context
            .expect("a fresh loader can select the new shards");
        assert!(matches!(
            setup.availability,
            SemanticAvailability::HashFallback
        ));
        assert_eq!(context.artifacts.len(), 1);
        assert_eq!(context.artifacts[0].fsvi_path(), shard_path);
    }

    #[test]
    fn admitted_shard_freshness_uses_its_retained_fingerprint_and_identity_marker() {
        use crate::search::asset_state::{SemanticPreference, semantic_state_from_availability};

        let tmp = tempdir().expect("retained cohort fixture");
        let db_path = tmp.path().join("cass.db");
        let storage = FrankenStorage::open(&db_path).expect("create cass db");
        let current = crate::indexer::lexical_storage_fingerprint_for_storage(&storage)
            .expect("fingerprint current database");
        let embedder = HashEmbedder::default();
        let old_path = publish_hash_shard_generation(
            tmp.path(),
            "older-archive",
            "vector_index/shards/hash/old-cohort.fsvi",
        );
        let admitted = load_complete_shard_artifacts_with_fingerprint(
            tmp.path(),
            embedder.id(),
            embedder.dimension(),
            "test",
            || Ok("older-archive".to_string()),
        )
        .expect("admit cohort against its original database fingerprint")
        .expect("complete original cohort");
        assert_eq!(admitted.db_fingerprint, "older-archive");
        assert_eq!(admitted.artifacts[0].fsvi_path(), old_path);

        let current_path = publish_hash_shard_generation(
            tmp.path(),
            &current,
            "vector_index/shards/hash/current-cohort.fsvi",
        );
        let status = semantic_state_from_availability(
            tmp.path(),
            &SemanticAvailability::HashFallback,
            SemanticPreference::HashFallback,
            Some(&current),
        );
        assert!(status.can_search);
        assert_eq!(
            status.vector_index_path.as_deref(),
            Some(current_path.as_path())
        );
        let refusal = refuse_stale_semantic_assets(
            tmp.path(),
            &storage,
            embedder.id(),
            &SemanticAvailability::HashFallback,
            SemanticPreference::HashFallback,
            Some(&admitted.db_fingerprint),
        )
        .expect("current on-disk metadata cannot refresh the already admitted old cohort");
        assert!(refusal.is_index_stale(), "{refusal:?}");

        let current_admission = load_complete_shard_artifacts_for_storage(
            tmp.path(),
            &storage,
            embedder.id(),
            embedder.dimension(),
            "test",
        )
        .expect("admit current cohort")
        .expect("complete current cohort");
        assert_eq!(current_admission.db_fingerprint, current);
        assert_eq!(current_admission.artifacts[0].fsvi_path(), current_path);
        assert!(
            refuse_stale_semantic_assets(
                tmp.path(),
                &storage,
                embedder.id(),
                &SemanticAvailability::HashFallback,
                SemanticPreference::HashFallback,
                Some(&current_admission.db_fingerprint),
            )
            .is_none()
        );

        storage
            .mark_semantic_identity_rebuild_required(SemanticIdentityTier::Fast)
            .expect("invalidate canonical filter identity");
        let refusal = refuse_stale_semantic_assets(
            tmp.path(),
            &storage,
            embedder.id(),
            &SemanticAvailability::HashFallback,
            SemanticPreference::HashFallback,
            Some(&current_admission.db_fingerprint),
        )
        .expect("matching fingerprints must still honor canonical identity invalidation");
        assert!(
            matches!(refusal, SemanticAvailability::IndexStale { reason, .. }
                if reason.contains("canonical agent/source identity changed"))
        );
    }

    #[test]
    fn freshness_fingerprint_failure_is_not_hidden_by_readable_filter_maps() {
        use crate::search::asset_state::SemanticPreference;

        let tmp = tempdir().expect("fingerprint failure fixture");
        let storage = FrankenStorage::open(&tmp.path().join("cass.db")).expect("create cass db");
        let authority = legacy::capture(&storage).expect("capture canonical authority");
        let current = crate::indexer::lexical_storage_fingerprint_for_storage(&storage)
            .expect("initial fingerprint succeeds");
        storage
            .raw()
            .execute("PRAGMA foreign_keys = OFF")
            .expect("allow fixture damage");
        storage
            .raw()
            .execute("DROP TABLE messages")
            .expect("make message fingerprint unavailable");
        assert!(crate::indexer::lexical_storage_fingerprint_for_storage(&storage).is_err());
        legacy::validate(&storage, &authority).expect("canonical authority remains readable");
        SemanticFilterMaps::from_storage(&storage).expect("filter maps do not read messages");

        for admitted_fingerprint in [None, Some(current.as_str())] {
            let refusal = refuse_stale_semantic_assets(
                tmp.path(),
                &storage,
                HashEmbedder::default().id(),
                &SemanticAvailability::HashFallback,
                SemanticPreference::HashFallback,
                admitted_fingerprint,
            )
            .expect("unverifiable freshness must refuse both monoliths and admitted shards");
            assert!(
                matches!(refusal, SemanticAvailability::LoadFailed { context }
                    if context.contains("could not fingerprint the current DB"))
            );
        }
    }

    #[test]
    fn load_hash_context_preserves_legacy_fallback_without_selected_shards() {
        for scenario in ["absent", "unrelated", "incomplete", "older_generation"] {
            let tmp = tempdir().unwrap();
            let db_path = tmp.path().join("cass.db");
            let storage = FrankenStorage::open(&db_path).expect("create cass db");
            drop(storage);
            let db_fingerprint = crate::indexer::lexical_storage_fingerprint_for_db(&db_path)
                .expect("fingerprint cass db");
            let embedder = HashEmbedder::default();
            let monolithic_path = vector_index_path(tmp.path(), embedder.id());
            write_hash_vector_index(&monolithic_path, 1);

            if scenario != "absent" {
                let (embedder_id, fingerprint, shard_count) = match scenario {
                    "unrelated" => ("other-384", db_fingerprint.as_str(), 1),
                    "incomplete" => (embedder.id(), db_fingerprint.as_str(), 2),
                    "older_generation" => (embedder.id(), "fp-older-archive", 1),
                    _ => unreachable!(),
                };
                let mut manifest = SemanticShardManifest {
                    shards: vec![semantic_shard_record(
                        TierKind::Fast,
                        embedder_id,
                        fingerprint,
                        0,
                        shard_count,
                    )],
                    ..Default::default()
                };
                manifest
                    .save(tmp.path())
                    .expect("save nonselected shard manifest");
            }

            let setup = load_hash_semantic_context(tmp.path(), &db_path);
            assert!(
                matches!(setup.availability, SemanticAvailability::HashFallback),
                "{scenario}: {:?}",
                setup.availability
            );
            let context = setup
                .context
                .expect("a nonselected cohort permits legacy serving");
            assert_eq!(context.artifacts.len(), 1, "{scenario}");
            assert_eq!(
                context.artifacts[0].fsvi_path(),
                monolithic_path,
                "{scenario}"
            );
        }
    }

    #[test]
    fn load_hash_context_refuses_failed_selected_shards_instead_of_old_monolith() {
        use crate::search::semantic_manifest::SemanticManifest;

        for failure in [
            "corrupt",
            "missing",
            "unsafe_path",
            "wrong_dimension",
            "wrong_revision",
            "wrong_embedder",
            "duplicate_index",
            "inconsistent_count",
            "inconsistent_revision",
            "malformed_manifest",
        ] {
            let tmp = tempdir().unwrap();
            let db_path = tmp.path().join("cass.db");
            let storage = FrankenStorage::open(&db_path).expect("create cass db");
            drop(storage);
            let db_fingerprint = crate::indexer::lexical_storage_fingerprint_for_db(&db_path)
                .expect("fingerprint cass db");
            let embedder = HashEmbedder::default();
            let monolithic_path = vector_index_path(tmp.path(), embedder.id());
            write_hash_vector_index(&monolithic_path, 1);
            assert!(
                load_hash_semantic_context(tmp.path(), &db_path)
                    .context
                    .is_some(),
                "{failure}: the fallback artifact must be independently loadable"
            );

            let mut legacy_manifest =
                SemanticManifest::load_or_default(tmp.path()).expect("load legacy manifest");
            assert!(legacy_manifest.adopt_legacy_artifact(
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
            legacy_manifest
                .save(tmp.path())
                .expect("save older monolithic generation");

            let mut records = Vec::new();
            for shard_index in 0..2 {
                let mut record = semantic_shard_record(
                    TierKind::Fast,
                    embedder.id(),
                    &db_fingerprint,
                    shard_index,
                    2,
                );
                record.model_revision = "hash".to_string();
                let shard_path = tmp.path().join(&record.index_path);
                if shard_index == 1 && failure == "missing" {
                    // Keep the published record while leaving its artifact absent.
                } else if shard_index == 1 && failure.starts_with("wrong_") {
                    std::fs::create_dir_all(shard_path.parent().expect("shard parent"))
                        .expect("create shard parent");
                    let actual_embedder = if failure == "wrong_embedder" {
                        "other-384"
                    } else {
                        embedder.id()
                    };
                    let actual_revision = if failure == "wrong_revision" {
                        "legacy-hash-revision"
                    } else {
                        crate::indexer::semantic::HASH_VECTOR_SPACE_REVISION
                    };
                    let actual_dimension = if failure == "wrong_dimension" {
                        embedder.dimension() / 2
                    } else {
                        embedder.dimension()
                    };
                    let mut writer = VectorIndex::create_with_revision(
                        &shard_path,
                        actual_embedder,
                        actual_revision,
                        actual_dimension,
                        frankensearch::index::Quantization::F16,
                    )
                    .expect("create incompatible but readable shard");
                    let mut vector = vec![0.0; actual_dimension];
                    vector[0] = 1.0;
                    writer
                        .write_record("incompatible-doc", &vector)
                        .expect("write incompatible shard");
                    writer.finish().expect("finish incompatible shard");
                } else {
                    write_hash_vector_index(&shard_path, 1);
                }
                if shard_index == 1 {
                    match failure {
                        "corrupt" => std::fs::write(&shard_path, b"not an FSVI")
                            .expect("corrupt selected shard"),
                        "unsafe_path" => record.index_path = "../outside.fsvi".to_string(),
                        "duplicate_index" => record.shard_index = 0,
                        "inconsistent_count" => record.shard_count = 3,
                        "inconsistent_revision" => {
                            record.model_revision = "other-revision".to_string()
                        }
                        _ => {}
                    }
                }
                records.push(record);
            }
            let mut manifest = SemanticShardManifest {
                shards: records,
                ..Default::default()
            };
            manifest
                .save(tmp.path())
                .expect("save current selected shard generation");
            if failure == "malformed_manifest" {
                std::fs::write(SemanticShardManifest::path(tmp.path()), b"not json")
                    .expect("corrupt shard manifest");
            }

            let setup = load_hash_semantic_context(tmp.path(), &db_path);
            assert!(
                setup.context.is_none(),
                "{failure}: failed selected shards must not serve the older monolith"
            );
            let SemanticAvailability::LoadFailed { context } = setup.availability else {
                panic!(
                    "{failure}: expected shard admission failure, got {:?}",
                    setup.availability
                );
            };
            assert!(context.contains("shard"), "{failure}: {context}");
            assert!(
                context.contains("rebuild semantic vectors"),
                "{failure}: {context}"
            );
            if failure.starts_with("wrong_") {
                assert!(
                    context.contains("incompatible vector index"),
                    "{failure}: {context}"
                );
            }
        }
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
