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
use crate::search::model_download::{ModelManifest, ModelState, check_version_mismatch};
use crate::search::vector_index::{
    ROLE_ASSISTANT, ROLE_USER, SemanticFilterMaps, VectorIndex, vector_index_path,
};
use crate::storage::sqlite::SqliteStorage;

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
    pub filter_maps: SemanticFilterMaps,
    pub roles: Option<HashSet<u8>>,
}

pub struct SemanticSetup {
    pub availability: SemanticAvailability,
    pub context: Option<SemanticContext>,
}

/// Load semantic context with optional version mismatch checking.
///
/// If `check_for_updates` is true, this function will check if the installed
/// model version matches the manifest and return `UpdateAvailable` if they differ.
pub fn load_semantic_context(data_dir: &Path, db_path: &Path) -> SemanticSetup {
    load_semantic_context_inner(data_dir, db_path, true)
}

/// Load hash-based semantic context (no model download required).
pub fn load_hash_semantic_context(data_dir: &Path, db_path: &Path) -> SemanticSetup {
    let embedder = HashEmbedder::default();
    let index_path = vector_index_path(data_dir, embedder.id());
    if !index_path.is_file() {
        return SemanticSetup {
            availability: SemanticAvailability::IndexMissing { index_path },
            context: None,
        };
    }

    let storage = match SqliteStorage::open_readonly(db_path) {
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

    let index = match VectorIndex::load(&index_path) {
        Ok(index) => index,
        Err(err) => {
            return SemanticSetup {
                availability: SemanticAvailability::LoadFailed {
                    context: format!("vector index: {err}"),
                },
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
            index,
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
    load_semantic_context_inner(data_dir, db_path, false)
}

fn load_semantic_context_inner(
    data_dir: &Path,
    db_path: &Path,
    check_for_updates: bool,
) -> SemanticSetup {
    let model_dir = FastEmbedder::default_model_dir(data_dir);
    let missing_files = FastEmbedder::required_model_files()
        .iter()
        .filter(|name| !model_dir.join(*name).is_file())
        .map(|name| (*name).to_string())
        .collect::<Vec<_>>();

    if !missing_files.is_empty() {
        return SemanticSetup {
            availability: SemanticAvailability::ModelMissing {
                model_dir,
                missing_files,
            },
            context: None,
        };
    }

    // Check for model version mismatch
    if check_for_updates {
        let manifest = ModelManifest::minilm_v2();
        if let Some(ModelState::UpdateAvailable {
            current_revision,
            latest_revision,
        }) = check_version_mismatch(&model_dir, &manifest)
        {
            return SemanticSetup {
                availability: SemanticAvailability::UpdateAvailable {
                    embedder_id: FastEmbedder::embedder_id_static().to_string(),
                    current_revision,
                    latest_revision,
                },
                context: None,
            };
        }
    }

    let index_path = vector_index_path(data_dir, FastEmbedder::embedder_id_static());
    if !index_path.is_file() {
        return SemanticSetup {
            availability: SemanticAvailability::IndexMissing { index_path },
            context: None,
        };
    }

    let storage = match SqliteStorage::open_readonly(db_path) {
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

    let index = match VectorIndex::load(&index_path) {
        Ok(index) => index,
        Err(err) => {
            return SemanticSetup {
                availability: SemanticAvailability::LoadFailed {
                    context: format!("vector index: {err}"),
                },
                context: None,
            };
        }
    };

    let embedder = match FastEmbedder::load_from_dir(&model_dir) {
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
            filter_maps,
            roles,
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
    match VectorIndex::load(&index_path) {
        Ok(index) => {
            // Check if the index was built with a different embedder
            // The vector index stores the embedder ID in its header
            let expected_id = FastEmbedder::embedder_id_static();
            index.header().embedder_id != expected_id
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
        let not_installed = SemanticAvailability::NotInstalled;
        assert!(not_installed.is_not_installed());
        assert_eq!(not_installed.status_label(), "LEX");

        let needs_consent = SemanticAvailability::NeedsConsent;
        assert!(needs_consent.needs_consent());
        assert_eq!(needs_consent.status_label(), "LEX");

        let verifying = SemanticAvailability::Verifying;
        assert!(verifying.summary().contains("verifying"));
        assert_eq!(verifying.status_label(), "VFY...");

        let hash_fallback = SemanticAvailability::HashFallback;
        assert!(hash_fallback.is_hash_fallback());
        assert!(hash_fallback.can_search());
        assert_eq!(hash_fallback.status_label(), "SEM*");

        let disabled = SemanticAvailability::Disabled {
            reason: "offline mode".into(),
        };
        assert!(disabled.is_disabled());
        assert!(disabled.summary().contains("offline"));
        assert_eq!(disabled.status_label(), "OFF");
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
}
