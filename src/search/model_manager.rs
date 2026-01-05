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
use crate::search::model_download::{check_version_mismatch, ModelManifest, ModelState};
use crate::search::vector_index::{
    ROLE_ASSISTANT, ROLE_USER, SemanticFilterMaps, VectorIndex, vector_index_path,
};
use crate::storage::sqlite::SqliteStorage;

#[derive(Debug, Clone)]
pub enum SemanticAvailability {
    /// Model is ready for use.
    Ready {
        embedder_id: String,
    },
    /// Model files are missing.
    ModelMissing {
        model_dir: PathBuf,
        missing_files: Vec<String>,
    },
    /// Vector index is missing.
    IndexMissing {
        index_path: PathBuf,
    },
    /// Database is unavailable.
    DatabaseUnavailable {
        db_path: PathBuf,
        error: String,
    },
    /// Failed to load semantic context.
    LoadFailed {
        context: String,
    },
    /// Model update available - index rebuild needed.
    UpdateAvailable {
        embedder_id: String,
        current_revision: String,
        latest_revision: String,
    },
    /// Index is being rebuilt after model upgrade.
    IndexBuilding {
        embedder_id: String,
    },
}

impl SemanticAvailability {
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

    pub fn summary(&self) -> String {
        match self {
            SemanticAvailability::Ready { embedder_id } => {
                format!("semantic ready ({embedder_id})")
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
            SemanticAvailability::IndexBuilding { embedder_id } => {
                format!("rebuilding index for {embedder_id}")
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
    fn test_semantic_availability_summary() {
        let ready = SemanticAvailability::Ready {
            embedder_id: "test-123".into(),
        };
        assert!(ready.summary().contains("semantic ready"));
        assert!(ready.is_ready());
        assert!(!ready.has_update());

        let update = SemanticAvailability::UpdateAvailable {
            embedder_id: "test".into(),
            current_revision: "v1".into(),
            latest_revision: "v2".into(),
        };
        assert!(update.summary().contains("update available"));
        assert!(!update.is_ready());
        assert!(update.has_update());

        let building = SemanticAvailability::IndexBuilding {
            embedder_id: "test".into(),
        };
        assert!(building.summary().contains("rebuilding"));
        assert!(building.is_building());
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
