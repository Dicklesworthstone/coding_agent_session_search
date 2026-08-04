//! Semantic model management (local-only detection).
//!
//! This module wires the FastEmbed MiniLM embedder into semantic search by:
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

use crate::search::embedder::Embedder;
use crate::search::fastembed_embedder::FastEmbedder;
use crate::search::hash_embedder::HashEmbedder;
use crate::search::model_download::{
    ModelAcquisitionPolicy, ModelCacheState, ModelManifest, classify_model_cache,
    classify_model_cache_metadata,
};
use crate::search::policy::{CliSemanticOverrides, SemanticPolicy};
use crate::search::semantic_manifest::{
    SemanticShardManifest, SemanticShardRecord, TierKind, semantic_shard_artifact_path_is_safe,
};
use crate::search::vector_index::{
    ROLE_ASSISTANT, ROLE_USER, SemanticFilterMaps, VectorIndex, vector_index_path,
};
use crate::storage::sqlite::FrankenStorage;

/// Unified TUI state machine for semantic search availability.
///
/// This enum tracks the full lifecycle of semantic search from the user's perspective:
/// - Model installation flow (NotInstalled → NeedsConsent → Downloading → Verifying → Ready)
/// - Index building flow (Ready → IndexBuilding → Ready)
/// - User preferences (HashFallback, Disabled)
/// - Error states (LoadFailed, ModelMissing, etc.)
#[derive(Debug, Clone)]
pub enum SemanticAvailability {
    /// Model is ready for use.
    Ready { embedder_id: String },

    // =========================================================================
    // TUI-centric states for user flow
    // =========================================================================
    /// Model not installed - semantic not available.
    /// TUI should show option to download or use hash fallback.
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

    /// User opted for hash-based fallback (no ML model).
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

    /// Check if a download is in progress.
    pub fn is_downloading(&self) -> bool {
        matches!(self, SemanticAvailability::Downloading { .. })
    }

    /// Check if user consent is needed.
    pub fn needs_consent(&self) -> bool {
        matches!(self, SemanticAvailability::NeedsConsent)
    }

    /// Check if hash fallback is active.
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

    /// Check if semantic can be used (ready or hash fallback).
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
            SemanticAvailability::HashFallback => "using hash-based fallback".to_string(),
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
    pub index: VectorIndex,
    pub additional_indexes: Vec<VectorIndex>,
    pub filter_maps: SemanticFilterMaps,
    pub roles: Option<HashSet<u8>>,
}

pub struct SemanticSetup {
    pub availability: SemanticAvailability,
    pub context: Option<SemanticContext>,
}

fn semantic_sidecar_path(data_dir: &Path, recorded_path: &str) -> Option<PathBuf> {
    semantic_shard_artifact_path_is_safe(recorded_path).then(|| data_dir.join(recorded_path))
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
        if !path.is_file() {
            return Ok(None);
        }
    }

    Ok(Some(records))
}

fn load_complete_shard_indexes(
    data_dir: &Path,
    embedder_id: &str,
    db_fingerprint: &str,
) -> Result<Option<Vec<VectorIndex>>, String> {
    for tier in [TierKind::Quality, TierKind::Fast] {
        let Some(records) =
            matching_complete_shard_records(data_dir, tier, embedder_id, db_fingerprint)?
        else {
            continue;
        };

        let mut indexes = Vec::with_capacity(records.len());
        for shard in records {
            let Some(path) = semantic_sidecar_path(data_dir, &shard.index_path) else {
                return Ok(None);
            };
            let index = VectorIndex::open(&path)
                .map_err(|err| format!("semantic shard vector index {}: {err}", path.display()))?;
            if index.embedder_id() != embedder_id || index.dimension() != shard.dimension {
                return Err(format!(
                    "semantic shard vector index {} metadata mismatch",
                    path.display()
                ));
            }
            indexes.push(index);
        }
        if !indexes.is_empty() {
            tracing::info!(
                tier = tier.as_str(),
                embedder = embedder_id,
                shard_count = indexes.len(),
                "loaded complete semantic shard generation"
            );
            return Ok(Some(indexes));
        }
    }

    Ok(None)
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

fn load_complete_shard_indexes_for_current_db(
    data_dir: &Path,
    db_path: &Path,
    embedder_id: &str,
    context_label: &'static str,
) -> Option<Vec<VectorIndex>> {
    let db_fingerprint = match crate::indexer::lexical_storage_fingerprint_for_db(db_path) {
        Ok(fingerprint) => fingerprint,
        Err(err) => {
            tracing::debug!(
                error = %err,
                embedder = embedder_id,
                context = context_label,
                "semantic shard context unavailable: failed to fingerprint current DB"
            );
            return None;
        }
    };

    match load_complete_shard_indexes(data_dir, embedder_id, &db_fingerprint) {
        Ok(indexes) => indexes,
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

pub fn load_semantic_context_for_embedder(
    data_dir: &Path,
    db_path: &Path,
    embedder_name: &str,
) -> SemanticSetup {
    load_semantic_context_inner(data_dir, db_path, true, embedder_name)
}

/// Probe semantic availability without loading the embedder, vector index, or
/// DB-backed filter maps. Status/health surfaces use this to report readiness
/// cheaply; actual semantic search still calls `load_semantic_context`.
pub(crate) fn probe_semantic_availability(data_dir: &Path) -> SemanticAvailability {
    probe_semantic_availability_for_embedder(data_dir, active_policy_embedder_name())
}

pub(crate) fn probe_semantic_availability_for_embedder(
    data_dir: &Path,
    embedder_name: &str,
) -> SemanticAvailability {
    let canonical_name = FastEmbedder::canonical_name(embedder_name).unwrap_or("minilm");
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
    if !index_path.is_file() {
        return SemanticAvailability::IndexMissing { index_path };
    }

    SemanticAvailability::Ready {
        embedder_id: config.embedder_id,
    }
}

/// Probe hash semantic availability without opening the DB or vector index.
pub(crate) fn probe_hash_semantic_availability(data_dir: &Path) -> SemanticAvailability {
    let embedder = HashEmbedder::default();
    let index_path = vector_index_path(data_dir, embedder.id());
    if !index_path.is_file() {
        SemanticAvailability::IndexMissing { index_path }
    } else {
        SemanticAvailability::HashFallback
    }
}

/// Load hash-based semantic context (no model download required).
pub fn load_hash_semantic_context(data_dir: &Path, db_path: &Path) -> SemanticSetup {
    let embedder = HashEmbedder::default();
    let index_path = vector_index_path(data_dir, embedder.id());
    let monolithic_present = index_path.is_file();
    let shard_indexes = if monolithic_present
        || complete_shard_generation_candidate_exists(data_dir, embedder.id())
    {
        load_complete_shard_indexes_for_current_db(
            data_dir,
            db_path,
            embedder.id(),
            "hash semantic",
        )
    } else {
        None
    };
    if !monolithic_present && shard_indexes.is_none() {
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

    let (index, additional_indexes) = if let Some(mut indexes) = shard_indexes {
        let index = indexes.remove(0);
        (index, indexes)
    } else {
        match VectorIndex::open(&index_path) {
            Ok(index) => (index, Vec::new()),
            Err(err) => {
                return SemanticSetup {
                    availability: SemanticAvailability::LoadFailed {
                        context: format!("vector index: {err}"),
                    },
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
            index,
            additional_indexes,
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
    load_semantic_context_inner(data_dir, db_path, false, active_policy_embedder_name())
}

fn load_semantic_context_inner(
    data_dir: &Path,
    db_path: &Path,
    check_for_updates: bool,
    embedder_name: &str,
) -> SemanticSetup {
    let canonical_name = FastEmbedder::canonical_name(embedder_name).unwrap_or("minilm");
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
    let monolithic_present = index_path.is_file();
    let shard_indexes = if monolithic_present
        || complete_shard_generation_candidate_exists(data_dir, &config.embedder_id)
    {
        load_complete_shard_indexes_for_current_db(
            data_dir,
            db_path,
            &config.embedder_id,
            "semantic",
        )
    } else {
        None
    };
    if !monolithic_present && shard_indexes.is_none() {
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

    let (index, additional_indexes) = if let Some(mut indexes) = shard_indexes {
        let index = indexes.remove(0);
        (index, indexes)
    } else {
        match VectorIndex::open(&index_path) {
            Ok(index) => (index, Vec::new()),
            Err(err) => {
                return SemanticSetup {
                    availability: SemanticAvailability::LoadFailed {
                        context: format!("vector index: {err}"),
                    },
                    context: None,
                };
            }
        }
    };

    let embedder = match FastEmbedder::load_by_name(data_dir, canonical_name) {
        Ok(embedder) => Arc::new(embedder) as Arc<dyn Embedder>,
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
            index,
            additional_indexes,
            filter_maps,
            roles,
        }),
    }
}

fn active_policy_embedder_name() -> &'static str {
    let semantic_policy = SemanticPolicy::resolve(&CliSemanticOverrides::default());
    FastEmbedder::canonical_name(&semantic_policy.quality_tier_embedder).unwrap_or("minilm")
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
/// This compares the embedder ID in the vector index header with the expected
/// embedder ID. If they differ, the index was built with a different model
/// and needs to be rebuilt.
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
            // Check if the index was built with a different embedder
            // The vector index stores the embedder ID in its header
            let expected_id = FastEmbedder::embedder_id_static();
            index.embedder_id() != expected_id
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
            "hash",
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
            context.additional_indexes.len(),
            1,
            "complete current shards must not be shadowed by an older monolithic vector file"
        );
        let loaded_records = context.index.record_count()
            + context
                .additional_indexes
                .iter()
                .map(VectorIndex::record_count)
                .sum::<usize>();
        assert_eq!(loaded_records, 2);
    }
}
