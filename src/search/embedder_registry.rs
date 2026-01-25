//! Embedder registry for model selection (bd-2mbe).
//!
//! This module provides a registry of available embedding backends that allows:
//! - Listing available embedders with metadata
//! - Selecting embedder by name from CLI/config
//! - Validating model availability before use
//! - Providing a sensible default model
//!
//! # Supported Embedders
//!
//! | Name | ID | Dimension | Type | Notes |
//! |------|-----|-----------|------|-------|
//! | minilm | minilm-384 | 384 | ML | Default semantic embedder |
//! | hash | fnv1a-384 | 384 | Hash | Always available fallback |
//!
//! # Example
//!
//! ```ignore
//! use crate::search::embedder_registry::{EmbedderRegistry, get_embedder};
//!
//! let registry = EmbedderRegistry::new(&data_dir);
//!
//! // List available embedders
//! for info in registry.available() {
//!     println!("{}: {} ({})", info.name, info.id, info.dimension);
//! }
//!
//! // Get embedder by name
//! let embedder = get_embedder(&data_dir, Some("minilm"))?;
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::embedder::{Embedder, EmbedderError, EmbedderInfo, EmbedderResult};
use super::fastembed_embedder::FastEmbedder;
use super::hash_embedder::HashEmbedder;

/// Default embedder name when none specified.
pub const DEFAULT_EMBEDDER: &str = "minilm";

/// Hash embedder name (always available).
pub const HASH_EMBEDDER: &str = "hash";

/// Information about a registered embedder.
#[derive(Debug, Clone)]
pub struct RegisteredEmbedder {
    /// Short name for CLI/config (e.g., "minilm", "hash").
    pub name: &'static str,
    /// Unique embedder ID (e.g., "minilm-384", "fnv1a-384").
    pub id: &'static str,
    /// Output dimension.
    pub dimension: usize,
    /// Whether this is a semantic (ML) embedder.
    pub is_semantic: bool,
    /// Human-readable description.
    pub description: &'static str,
    /// Whether the model files are required (false = always available).
    pub requires_model_files: bool,
}

impl RegisteredEmbedder {
    /// Check if this embedder is available in the given data directory.
    pub fn is_available(&self, data_dir: &Path) -> bool {
        if !self.requires_model_files {
            return true;
        }

        // Check if model files exist
        match self.name {
            "minilm" => {
                let model_dir = FastEmbedder::default_model_dir(data_dir);
                FastEmbedder::required_model_files()
                    .iter()
                    .all(|f| model_dir.join(f).is_file())
            }
            _ => false,
        }
    }

    /// Get the model directory path for this embedder (if applicable).
    pub fn model_dir(&self, data_dir: &Path) -> Option<PathBuf> {
        match self.name {
            "minilm" => Some(FastEmbedder::default_model_dir(data_dir)),
            _ => None,
        }
    }

    /// Get missing model files for this embedder.
    pub fn missing_files(&self, data_dir: &Path) -> Vec<String> {
        if !self.requires_model_files {
            return Vec::new();
        }

        match self.name {
            "minilm" => {
                let model_dir = FastEmbedder::default_model_dir(data_dir);
                FastEmbedder::required_model_files()
                    .iter()
                    .filter(|f| !model_dir.join(*f).is_file())
                    .map(|f| (*f).to_string())
                    .collect()
            }
            _ => Vec::new(),
        }
    }
}

/// Static registry of all supported embedders.
pub static EMBEDDERS: &[RegisteredEmbedder] = &[
    RegisteredEmbedder {
        name: "minilm",
        id: "minilm-384",
        dimension: 384,
        is_semantic: true,
        description: "MiniLM L6 v2 - fast, high-quality semantic embeddings",
        requires_model_files: true,
    },
    RegisteredEmbedder {
        name: "hash",
        id: "fnv1a-384",
        dimension: 384,
        is_semantic: false,
        description: "FNV-1a feature hashing - lexical fallback, always available",
        requires_model_files: false,
    },
];

/// Embedder registry with data directory context.
pub struct EmbedderRegistry {
    data_dir: PathBuf,
}

impl EmbedderRegistry {
    /// Create a new registry bound to the given data directory.
    pub fn new(data_dir: &Path) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
        }
    }

    /// Get all registered embedders.
    pub fn all(&self) -> &'static [RegisteredEmbedder] {
        EMBEDDERS
    }

    /// Get only available embedders (model files present).
    pub fn available(&self) -> Vec<&'static RegisteredEmbedder> {
        EMBEDDERS
            .iter()
            .filter(|e| e.is_available(&self.data_dir))
            .collect()
    }

    /// Get embedder info by name.
    pub fn get(&self, name: &str) -> Option<&'static RegisteredEmbedder> {
        let name_lower = name.to_ascii_lowercase();
        EMBEDDERS.iter().find(|e| {
            e.name == name_lower
                || e.id == name_lower
                || e.id.starts_with(&format!("{}-", name_lower))
        })
    }

    /// Check if an embedder is available by name.
    pub fn is_available(&self, name: &str) -> bool {
        self.get(name)
            .map(|e| e.is_available(&self.data_dir))
            .unwrap_or(false)
    }

    /// Get the default embedder info.
    pub fn default_embedder(&self) -> &'static RegisteredEmbedder {
        self.get(DEFAULT_EMBEDDER)
            .expect("default embedder must exist")
    }

    /// Get the best available embedder (ML if available, hash fallback).
    pub fn best_available(&self) -> &'static RegisteredEmbedder {
        // Try ML embedders first
        for e in EMBEDDERS.iter().filter(|e| e.is_semantic) {
            if e.is_available(&self.data_dir) {
                return e;
            }
        }
        // Fall back to hash
        self.get(HASH_EMBEDDER).expect("hash embedder must exist")
    }

    /// Validate that an embedder is ready to use.
    ///
    /// Returns `Ok(())` if available, or an error with details about what's missing.
    pub fn validate(&self, name: &str) -> EmbedderResult<&'static RegisteredEmbedder> {
        let embedder = self.get(name).ok_or_else(|| {
            EmbedderError::Unavailable(format!(
                "unknown embedder '{}'. Available: {}",
                name,
                EMBEDDERS
                    .iter()
                    .map(|e| e.name)
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;

        if !embedder.is_available(&self.data_dir) {
            let missing = embedder.missing_files(&self.data_dir);
            let model_dir = embedder
                .model_dir(&self.data_dir)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "unknown".to_string());

            return Err(EmbedderError::Unavailable(format!(
                "embedder '{}' not available: missing files in {}: {}. Run 'cass models install' to download.",
                name,
                model_dir,
                missing.join(", ")
            )));
        }

        Ok(embedder)
    }
}

/// Load an embedder by name (or default if None).
///
/// # Arguments
///
/// * `data_dir` - The cass data directory containing model files.
/// * `name` - Optional embedder name. If None, uses the best available.
///
/// # Returns
///
/// An `Arc<dyn Embedder>` ready for use, or an error if unavailable.
pub fn get_embedder(data_dir: &Path, name: Option<&str>) -> EmbedderResult<Arc<dyn Embedder>> {
    let registry = EmbedderRegistry::new(data_dir);

    let embedder_info = match name {
        Some(n) => registry.validate(n)?,
        None => registry.best_available(),
    };

    load_embedder_by_name(data_dir, embedder_info.name)
}

/// Load an embedder by registered name.
fn load_embedder_by_name(data_dir: &Path, name: &str) -> EmbedderResult<Arc<dyn Embedder>> {
    match name {
        "minilm" => {
            let model_dir = FastEmbedder::default_model_dir(data_dir);
            let embedder = FastEmbedder::load_from_dir(&model_dir)?;
            Ok(Arc::new(embedder))
        }
        "hash" => {
            let embedder = HashEmbedder::default();
            Ok(Arc::new(embedder))
        }
        _ => Err(EmbedderError::Unavailable(format!(
            "embedder '{}' not implemented",
            name
        ))),
    }
}

/// Get embedder info for display/logging.
pub fn get_embedder_info(data_dir: &Path, name: Option<&str>) -> Option<EmbedderInfo> {
    let registry = EmbedderRegistry::new(data_dir);

    let embedder_info = match name {
        Some(n) => registry.get(n)?,
        None => registry.best_available(),
    };

    Some(EmbedderInfo {
        id: embedder_info.id.to_string(),
        dimension: embedder_info.dimension,
        is_semantic: embedder_info.is_semantic,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_registry_all() {
        let tmp = tempdir().unwrap();
        let registry = EmbedderRegistry::new(tmp.path());
        assert!(registry.all().len() >= 2);
    }

    #[test]
    fn test_registry_get_by_name() {
        let tmp = tempdir().unwrap();
        let registry = EmbedderRegistry::new(tmp.path());

        let minilm = registry.get("minilm");
        assert!(minilm.is_some());
        assert_eq!(minilm.unwrap().dimension, 384);

        let hash = registry.get("hash");
        assert!(hash.is_some());
        assert_eq!(hash.unwrap().dimension, 384);

        let unknown = registry.get("unknown");
        assert!(unknown.is_none());
    }

    #[test]
    fn test_registry_get_by_id() {
        let tmp = tempdir().unwrap();
        let registry = EmbedderRegistry::new(tmp.path());

        let minilm = registry.get("minilm-384");
        assert!(minilm.is_some());
        assert_eq!(minilm.unwrap().name, "minilm");

        let hash = registry.get("fnv1a-384");
        assert!(hash.is_some());
        assert_eq!(hash.unwrap().name, "hash");
    }

    #[test]
    fn test_hash_always_available() {
        let tmp = tempdir().unwrap();
        let registry = EmbedderRegistry::new(tmp.path());

        assert!(registry.is_available("hash"));
        let available = registry.available();
        assert!(available.iter().any(|e| e.name == "hash"));
    }

    #[test]
    fn test_minilm_unavailable_without_files() {
        let tmp = tempdir().unwrap();
        let registry = EmbedderRegistry::new(tmp.path());

        // MiniLM should not be available without model files
        assert!(!registry.is_available("minilm"));

        let result = registry.validate("minilm");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, EmbedderError::Unavailable(_)));
    }

    #[test]
    fn test_best_available_fallback() {
        let tmp = tempdir().unwrap();
        let registry = EmbedderRegistry::new(tmp.path());

        // Without model files, best_available should return hash
        let best = registry.best_available();
        assert_eq!(best.name, "hash");
    }

    #[test]
    fn test_get_embedder_hash() {
        let tmp = tempdir().unwrap();
        let embedder = get_embedder(tmp.path(), Some("hash")).unwrap();
        assert_eq!(embedder.id(), "fnv1a-384");
        assert!(!embedder.is_semantic());
    }

    #[test]
    fn test_get_embedder_default_no_models() {
        let tmp = tempdir().unwrap();
        // Without model files, should fall back to hash
        let embedder = get_embedder(tmp.path(), None).unwrap();
        assert_eq!(embedder.id(), "fnv1a-384");
    }

    #[test]
    fn test_validate_unknown_embedder() {
        let tmp = tempdir().unwrap();
        let registry = EmbedderRegistry::new(tmp.path());

        let result = registry.validate("nonexistent");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("unknown embedder"));
        assert!(err.to_string().contains("Available:"));
    }

    #[test]
    fn test_registered_embedder_missing_files() {
        let tmp = tempdir().unwrap();
        let registry = EmbedderRegistry::new(tmp.path());

        let minilm = registry.get("minilm").unwrap();
        let missing = minilm.missing_files(tmp.path());
        assert!(!missing.is_empty());
        assert!(missing.contains(&"model.onnx".to_string()));
    }

    #[test]
    fn test_get_embedder_info() {
        let tmp = tempdir().unwrap();

        let hash_info = get_embedder_info(tmp.path(), Some("hash")).unwrap();
        assert_eq!(hash_info.id, "fnv1a-384");
        assert!(!hash_info.is_semantic);

        let minilm_info = get_embedder_info(tmp.path(), Some("minilm")).unwrap();
        assert_eq!(minilm_info.id, "minilm-384");
        assert!(minilm_info.is_semantic);
    }
}
