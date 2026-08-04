//! Embedder registry for model selection (bd-2mbe).
//!
//! This module provides a registry of available embedding backends that allows:
//! - Listing available embedders with metadata
//! - Selecting embedder by name from CLI/config
//! - Validating model availability before use
//! - Providing a sensible default model
//!
//! **Note**: The core types ([`RegisteredEmbedder`], [`EmbedderRegistry`]) are
//! structurally identical to those in `frankensearch_embed::model_registry`.
//! They are kept locally for now due to build-system sync constraints (rch does
//! not sync sibling path dependencies).  See frankensearch-embed for the
//! canonical definitions, which additionally include reranker support, two
//! additional Potion embedders, and richer directory-resolution helpers.
//!
//! # Supported Embedders
//!
//! | Name | ID | Dimension | Type | Notes |
//! |------|-----|-----------|------|-------|
//! | minilm | minilm-384 | 384 | ML | Default semantic embedder |
//! | hash | fnv1a-384 | 384 | Hash | Explicit fast/degraded mode |
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

/// Hash embedder name (always available when explicitly requested).
pub const HASH_EMBEDDER: &str = "hash";

/// Information about a registered embedder.
///
/// Structurally identical to `frankensearch_embed::model_registry::RegisteredEmbedder`.
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
    /// Release/update date (YYYY-MM-DD format) for bake-off eligibility.
    pub release_date: &'static str,
    /// HuggingFace model ID for download/reference.
    pub huggingface_id: &'static str,
    /// Approximate model size in bytes.
    pub size_bytes: u64,
    /// Whether this is a baseline model (not eligible for bake-off).
    pub is_baseline: bool,
}

/// Files required for the pure-Rust (frankentorch) embedder: a safetensors
/// weight file plus the tokenizer. The ONNX backend was removed in cass #308.
pub const REQUIRED_NATIVE_MODEL_FILES: &[&str] = &["model.safetensors", "tokenizer.json"];

/// Eligibility cutoff for bake-off (models must be released on/after this date).
pub const BAKEOFF_ELIGIBILITY_CUTOFF: &str = "2025-11-01";

impl RegisteredEmbedder {
    /// Check if this embedder is available in the given data directory.
    pub fn is_available(&self, data_dir: &Path) -> bool {
        if !self.requires_model_files {
            return true;
        }

        if let Some(model_dir) = self.model_dir(data_dir) {
            self.required_files()
                .iter()
                .all(|f| model_dir.join(f).is_file())
        } else {
            false
        }
    }

    /// Get the model directory path for this embedder (if applicable).
    pub fn model_dir(&self, data_dir: &Path) -> Option<PathBuf> {
        if !self.requires_model_files {
            return None;
        }

        FastEmbedder::model_dir_for(data_dir, self.name)
    }

    /// Get required model files for this embedder.
    pub fn required_files(&self) -> &'static [&'static str] {
        if !self.requires_model_files {
            return &[];
        }
        // The native MiniLM embedder uses a safetensors bundle.
        REQUIRED_NATIVE_MODEL_FILES
    }

    /// Get missing model files for this embedder.
    pub fn missing_files(&self, data_dir: &Path) -> Vec<String> {
        if !self.requires_model_files {
            return Vec::new();
        }

        if let Some(model_dir) = self.model_dir(data_dir) {
            self.required_files()
                .iter()
                .filter(|f| !model_dir.join(*f).is_file())
                .map(|f| (*f).to_string())
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Check if this embedder is eligible for the bake-off.
    pub fn is_bakeoff_eligible(&self) -> bool {
        if self.is_baseline {
            return false;
        }
        self.release_date >= BAKEOFF_ELIGIBILITY_CUTOFF
    }

    /// Convert to bakeoff ModelMetadata.
    pub fn to_model_metadata(&self) -> crate::bakeoff::ModelMetadata {
        crate::bakeoff::ModelMetadata {
            id: self.id.to_string(),
            name: self.name.to_string(),
            source: self.huggingface_id.to_string(),
            release_date: self.release_date.to_string(),
            dimension: Some(self.dimension),
            size_bytes: if self.size_bytes > 0 {
                Some(self.size_bytes)
            } else {
                None
            },
            is_baseline: self.is_baseline,
        }
    }
}

/// Static registry of all supported embedders.
///
/// MiniLM is the only architecture verified by the pure-Rust native backend.
/// Hash vectors remain available for the explicit fast/degraded tier, but are
/// never selected as a silent substitute for a missing semantic model.
pub static EMBEDDERS: &[RegisteredEmbedder] = &[
    // === Baseline (not eligible for bake-off) ===
    RegisteredEmbedder {
        name: "minilm",
        id: "minilm-384",
        dimension: 384,
        is_semantic: true,
        description: "MiniLM L6 v2 - fast, high-quality semantic embeddings (baseline)",
        requires_model_files: true,
        release_date: "2022-08-01",
        huggingface_id: "sentence-transformers/all-MiniLM-L6-v2",
        size_bytes: 90_000_000,
        is_baseline: true,
    },
    // === Explicit fast/degraded tier (always available) ===
    RegisteredEmbedder {
        name: "hash",
        id: "fnv1a-384",
        dimension: 384,
        is_semantic: false,
        description: "FNV-1a feature hashing - explicit fast/degraded vectors, always available",
        requires_model_files: false,
        release_date: "2020-01-01",
        huggingface_id: "",
        size_bytes: 0,
        is_baseline: true,
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
        let name_lower = FastEmbedder::canonical_name(name)
            .unwrap_or_else(|| name.trim())
            .to_ascii_lowercase();
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

    /// Get the preferred semantic embedder.
    ///
    /// If MiniLM is not installed this still returns its registry entry, so a
    /// caller either reports the missing model or fails open to lexical search.
    /// Hash vectors must be selected explicitly by name or tier.
    pub fn best_available(&self) -> &'static RegisteredEmbedder {
        for e in EMBEDDERS.iter().filter(|e| e.is_semantic) {
            if e.is_available(&self.data_dir) {
                return e;
            }
        }
        self.default_embedder()
    }

    /// Get all bake-off eligible embedders.
    pub fn bakeoff_eligible(&self) -> Vec<&'static RegisteredEmbedder> {
        EMBEDDERS
            .iter()
            .filter(|e| e.is_bakeoff_eligible())
            .collect()
    }

    /// Get available bake-off eligible embedders (model files present).
    pub fn available_bakeoff_candidates(&self) -> Vec<&'static RegisteredEmbedder> {
        EMBEDDERS
            .iter()
            .filter(|e| e.is_bakeoff_eligible() && e.is_available(&self.data_dir))
            .collect()
    }

    /// Get the baseline embedder for bake-off comparison.
    pub fn baseline_embedder(&self) -> Option<&'static RegisteredEmbedder> {
        EMBEDDERS.iter().find(|e| e.is_baseline)
    }

    /// Validate that an embedder is ready to use.
    ///
    /// Returns `Ok(())` if available, or an error with details about what's missing.
    pub fn validate(&self, name: &str) -> EmbedderResult<&'static RegisteredEmbedder> {
        let embedder = self.get(name).ok_or_else(|| {
            embedder_unavailable(
                name,
                format!(
                    "unknown embedder. Available: {}",
                    EMBEDDERS
                        .iter()
                        .map(|e| e.name)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )
        })?;

        if !embedder.is_available(&self.data_dir) {
            let model_dir = FastEmbedder::runtime_model_dir_for(&self.data_dir, embedder.name);
            let missing = model_dir
                .as_ref()
                .map(|dir| {
                    embedder
                        .required_files()
                        .iter()
                        .filter(|file| !dir.join(*file).is_file())
                        .map(|file| (*file).to_string())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_else(|| embedder.missing_files(&self.data_dir));
            if missing.is_empty() {
                return Ok(embedder);
            }
            let model_dir = model_dir
                .or_else(|| embedder.model_dir(&self.data_dir))
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "unknown".to_string());

            return Err(embedder_unavailable(
                name,
                format!(
                    "missing files in {}: {}. Run 'cass models install' to download.",
                    model_dir,
                    missing.join(", ")
                ),
            ));
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
        "hash" => {
            let embedder = HashEmbedder::default();
            Ok(Arc::new(embedder))
        }
        "minilm" => {
            let embedder = FastEmbedder::load_by_name(data_dir, name)?;
            Ok(Arc::new(embedder))
        }
        _ => Err(embedder_unavailable(name, "embedder not implemented")),
    }
}

fn embedder_unavailable(model: &str, reason: impl Into<String>) -> EmbedderError {
    EmbedderError::EmbedderUnavailable {
        model: model.to_string(),
        reason: reason.into(),
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
    use tempfile::{TempDir, tempdir};

    fn registry_fixture() -> (TempDir, EmbedderRegistry) {
        let tmp = tempdir().unwrap();
        let registry = EmbedderRegistry::new(tmp.path());
        (tmp, registry)
    }

    #[test]
    fn test_registry_all() {
        let (_tmp, registry) = registry_fixture();
        assert!(registry.all().len() >= 2);
    }

    #[test]
    fn test_registry_get_by_name() {
        let (_tmp, registry) = registry_fixture();

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
        let (_tmp, registry) = registry_fixture();

        let minilm = registry.get("minilm-384");
        assert!(minilm.is_some());
        assert_eq!(minilm.unwrap().name, "minilm");

        let hash = registry.get("fnv1a-384");
        assert!(hash.is_some());
        assert_eq!(hash.unwrap().name, "hash");
    }

    #[test]
    fn test_hash_always_available() {
        let (_tmp, registry) = registry_fixture();

        assert!(registry.is_available("hash"));
        let available = registry.available();
        assert!(available.iter().any(|e| e.name == "hash"));
    }

    #[test]
    fn test_minilm_unavailable_without_files() {
        let (_tmp, registry) = registry_fixture();

        // MiniLM should not be available without model files
        assert!(!registry.is_available("minilm"));

        let result = registry.validate("minilm");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, EmbedderError::EmbedderUnavailable { .. }));
    }

    #[test]
    fn test_embedder_unavailable_helper_shape() {
        let err = embedder_unavailable("demo", "missing model");
        match err {
            EmbedderError::EmbedderUnavailable { model, reason } => {
                assert_eq!(model, "demo");
                assert_eq!(reason, "missing model");
            }
            other => panic!("unexpected error shape: {other:?}"),
        }
    }

    #[test]
    fn test_best_available_never_silently_substitutes_hash() {
        let (_tmp, registry) = registry_fixture();

        // A missing quality model must fail open at the search layer; selecting
        // a same-dimensional hash vector space here would silently mix contracts.
        let best = registry.best_available();
        assert_eq!(best.name, "minilm");
    }

    #[test]
    fn test_get_embedder_hash() {
        let tmp = tempdir().unwrap();
        let embedder = get_embedder(tmp.path(), Some("hash")).unwrap();
        assert_eq!(embedder.id(), "fnv1a-384");
        assert!(!embedder.is_semantic());
    }

    #[test]
    fn test_get_embedder_default_no_models_reports_missing_minilm() -> Result<(), String> {
        let tmp = tempdir().unwrap();
        let error = match get_embedder(tmp.path(), None) {
            Ok(_) => return Err("missing MiniLM silently selected an embedder".into()),
            Err(error) => error,
        };
        if !matches!(error, EmbedderError::EmbedderUnavailable { .. }) {
            return Err(format!("unexpected error shape: {error:?}"));
        }
        Ok(())
    }

    #[test]
    fn test_validate_unknown_embedder() {
        let (_tmp, registry) = registry_fixture();

        let result = registry.validate("nonexistent");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("unknown embedder"));
        assert!(err.to_string().contains("Available:"));
    }

    #[test]
    fn test_registered_embedder_missing_files() {
        let (tmp, registry) = registry_fixture();

        let minilm = registry.get("minilm").unwrap();
        let missing = minilm.missing_files(tmp.path());
        assert!(!missing.is_empty());
        assert!(missing.contains(&"model.safetensors".to_string()));
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

    // ==================== Bake-off Tests ====================

    #[test]
    fn test_no_unloadable_embedder_is_bakeoff_eligible() {
        let (_tmp, registry) = registry_fixture();

        let eligible = registry.bakeoff_eligible();
        assert!(eligible.is_empty());

        // MiniLM should NOT be in the eligible list (it's the baseline)
        assert!(
            !eligible.iter().any(|e| e.name == "minilm"),
            "minilm should not be in eligible list"
        );

        // Hash should NOT be in the eligible list (not semantic)
        assert!(
            !eligible.iter().any(|e| e.name == "hash"),
            "hash should not be in eligible list"
        );
    }

    #[test]
    fn test_baseline_embedder() {
        let (_tmp, registry) = registry_fixture();

        let baseline = registry.baseline_embedder();
        assert!(baseline.is_some());
        let baseline = baseline.unwrap();
        assert_eq!(baseline.name, "minilm");
        assert!(baseline.is_baseline);
        assert!(!baseline.is_bakeoff_eligible());
    }

    #[test]
    fn test_bakeoff_eligibility_by_date() {
        let (_tmp, registry) = registry_fixture();

        // MiniLM was released before cutoff (2022-08-01)
        let minilm = registry.get("minilm").unwrap();
        assert!(
            minilm.release_date < BAKEOFF_ELIGIBILITY_CUTOFF,
            "minilm should be released before cutoff"
        );

        // All eligible models should be released after cutoff
        for e in registry.bakeoff_eligible() {
            assert!(
                e.release_date >= BAKEOFF_ELIGIBILITY_CUTOFF,
                "{} should be released after cutoff (date: {})",
                e.name,
                e.release_date
            );
        }
    }

    #[test]
    fn test_bakeoff_model_metadata_conversion() {
        let (_tmp, registry) = registry_fixture();

        let minilm = registry.get("minilm").unwrap();
        let metadata = minilm.to_model_metadata();

        assert_eq!(metadata.id, "minilm-384");
        assert_eq!(metadata.name, "minilm");
        assert!(metadata.source.contains("MiniLM"));
        assert_eq!(metadata.release_date, "2022-08-01");
        assert_eq!(metadata.dimension, Some(384));
        assert!(metadata.is_baseline);
        assert!(!metadata.is_eligible());
    }

    #[test]
    fn test_unverified_architectures_are_not_registered() {
        let (_tmp, registry) = registry_fixture();
        assert!(registry.get("snowflake-arctic-s").is_none());
        assert!(registry.get("nomic-embed").is_none());
    }

    #[test]
    fn test_all_embedders_have_required_fields() {
        for e in EMBEDDERS.iter() {
            // All should have valid release dates
            assert!(
                !e.release_date.is_empty(),
                "{} should have a release date",
                e.name
            );

            // All semantic embedders should have HuggingFace IDs
            if e.is_semantic && e.requires_model_files {
                assert!(
                    !e.huggingface_id.is_empty(),
                    "{} should have a huggingface_id",
                    e.name
                );
            }

            // Dimensions should be reasonable
            assert!(e.dimension >= 256 && e.dimension <= 2048);
        }
    }

    #[test]
    fn test_model_dir_for_all_embedders() {
        let tmp = tempdir().unwrap();

        for e in EMBEDDERS.iter() {
            if e.requires_model_files {
                let dir = e.model_dir(tmp.path());
                assert!(dir.is_some(), "{} should have a model directory", e.name);
                let dir = dir.unwrap();
                assert!(
                    dir.starts_with(tmp.path().join("models")),
                    "{} model dir should be under models/",
                    e.name
                );
            }
        }
    }
}
