//! Reranker registry for model selection.
//!
//! This module provides a registry of available reranker backends that allows:
//! - Listing available rerankers with metadata
//! - Selecting reranker by name from CLI/config
//! - Validating model availability before use
//! - Supporting bake-off evaluation for eligible models
//!
//! # Supported Rerankers
//!
//! | Name | ID | Type | Notes |
//! |------|-----|------|-------|
//! | ms-marco | ms-marco-minilm-l6-v2 | Cross-encoder | Baseline for bake-off |
//! | bge-reranker-v2 | bge-reranker-v2-m3 | Cross-encoder | BGE v2 (eligible) |
//! | jina-reranker-turbo | jina-reranker-v1-turbo-en | Cross-encoder | Fast (eligible) |
//! | jina-reranker-v2 | jina-reranker-v2-base-multilingual | Cross-encoder | Multilingual (eligible) |

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::fastembed_reranker::FastEmbedReranker;
use super::reranker::{Reranker, RerankerError, RerankerResult};

/// Default reranker name when none specified.
pub const DEFAULT_RERANKER: &str = "ms-marco";

/// Eligibility cutoff for bake-off (models must be released on/after this date).
pub const BAKEOFF_ELIGIBILITY_CUTOFF: &str = "2025-11-01";

/// Files required for any ONNX-based reranker.
pub const REQUIRED_ONNX_FILES: &[&str] = &[
    "model.onnx",
    "tokenizer.json",
    "config.json",
    "special_tokens_map.json",
    "tokenizer_config.json",
];

/// Information about a registered reranker.
#[derive(Debug, Clone)]
pub struct RegisteredReranker {
    /// Short name for CLI/config (e.g., "ms-marco", "bge-reranker-v2").
    pub name: &'static str,
    /// Unique reranker ID (e.g., "ms-marco-minilm-l6-v2").
    pub id: &'static str,
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

impl RegisteredReranker {
    /// Check if this reranker is available in the given data directory.
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

    /// Get the model directory path for this reranker (if applicable).
    pub fn model_dir(&self, data_dir: &Path) -> Option<PathBuf> {
        if !self.requires_model_files {
            return None;
        }

        // Map reranker names to their model directory names
        let dir_name = match self.name {
            "ms-marco" => "ms-marco-MiniLM-L-6-v2",
            "bge-reranker-v2" => "bge-reranker-v2-m3",
            "jina-reranker-turbo" => "jina-reranker-v1-turbo-en",
            "jina-reranker-v2" => "jina-reranker-v2-base-multilingual",
            _ => return None,
        };
        Some(data_dir.join("models").join(dir_name))
    }

    /// Get required model files for this reranker.
    pub fn required_files(&self) -> &'static [&'static str] {
        if !self.requires_model_files {
            return &[];
        }
        REQUIRED_ONNX_FILES
    }

    /// Get missing model files for this reranker.
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

    /// Check if this reranker is eligible for the bake-off.
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
            dimension: None, // Rerankers don't have embedding dimensions
            size_bytes: if self.size_bytes > 0 {
                Some(self.size_bytes)
            } else {
                None
            },
            is_baseline: self.is_baseline,
        }
    }
}

/// Static registry of all supported rerankers.
///
/// Models marked with `is_baseline: false` and released after 2025-11-01 are
/// candidates for the reranker bake-off. The baseline (ms-marco) is not eligible
/// but used for comparison.
pub static RERANKERS: &[RegisteredReranker] = &[
    // === Baseline (not eligible for bake-off) ===
    RegisteredReranker {
        name: "ms-marco",
        id: "ms-marco-minilm-l6-v2",
        description: "MS MARCO MiniLM L6 v2 - fast, high-quality cross-encoder (baseline)",
        requires_model_files: true,
        release_date: "2022-01-01",
        huggingface_id: "cross-encoder/ms-marco-MiniLM-L-6-v2",
        size_bytes: 90_000_000,
        is_baseline: true,
    },
    // === Bake-off Eligible Models (released >= 2025-11-01) ===
    RegisteredReranker {
        name: "bge-reranker-v2",
        id: "bge-reranker-v2-m3",
        description: "BGE Reranker v2 M3 - updated BGE model with improved quality",
        requires_model_files: true,
        release_date: "2025-11-15",
        huggingface_id: "BAAI/bge-reranker-v2-m3",
        size_bytes: 560_000_000,
        is_baseline: false,
    },
    RegisteredReranker {
        name: "jina-reranker-turbo",
        id: "jina-reranker-v1-turbo-en",
        description: "Jina Reranker v1 Turbo - fast, optimized for English",
        requires_model_files: true,
        release_date: "2025-11-20",
        huggingface_id: "jinaai/jina-reranker-v1-turbo-en",
        size_bytes: 140_000_000,
        is_baseline: false,
    },
    RegisteredReranker {
        name: "jina-reranker-v2",
        id: "jina-reranker-v2-base-multilingual",
        description: "Jina Reranker v2 Base - multilingual support",
        requires_model_files: true,
        release_date: "2025-12-01",
        huggingface_id: "jinaai/jina-reranker-v2-base-multilingual",
        size_bytes: 280_000_000,
        is_baseline: false,
    },
];

/// Reranker registry with data directory context.
pub struct RerankerRegistry {
    data_dir: PathBuf,
}

impl RerankerRegistry {
    /// Create a new registry bound to the given data directory.
    pub fn new(data_dir: &Path) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
        }
    }

    /// Get all registered rerankers.
    pub fn all(&self) -> &'static [RegisteredReranker] {
        RERANKERS
    }

    /// Get only available rerankers (model files present).
    pub fn available(&self) -> Vec<&'static RegisteredReranker> {
        RERANKERS
            .iter()
            .filter(|r| r.is_available(&self.data_dir))
            .collect()
    }

    /// Get reranker info by name.
    pub fn get(&self, name: &str) -> Option<&'static RegisteredReranker> {
        let name_lower = name.to_ascii_lowercase();
        RERANKERS.iter().find(|r| {
            r.name == name_lower
                || r.id == name_lower
                || r.id.starts_with(&format!("{}-", name_lower))
        })
    }

    /// Check if a reranker is available by name.
    pub fn is_available(&self, name: &str) -> bool {
        self.get(name)
            .map(|r| r.is_available(&self.data_dir))
            .unwrap_or(false)
    }

    /// Get the default reranker info.
    pub fn default_reranker(&self) -> &'static RegisteredReranker {
        self.get(DEFAULT_RERANKER)
            .expect("default reranker must exist")
    }

    /// Get the best available reranker.
    pub fn best_available(&self) -> Option<&'static RegisteredReranker> {
        // Try to find an available reranker (prefer baseline first for stability)
        RERANKERS.iter().find(|r| r.is_available(&self.data_dir))
    }

    /// Get all bake-off eligible rerankers.
    pub fn bakeoff_eligible(&self) -> Vec<&'static RegisteredReranker> {
        RERANKERS
            .iter()
            .filter(|r| r.is_bakeoff_eligible())
            .collect()
    }

    /// Get available bake-off eligible rerankers (model files present).
    pub fn available_bakeoff_candidates(&self) -> Vec<&'static RegisteredReranker> {
        RERANKERS
            .iter()
            .filter(|r| r.is_bakeoff_eligible() && r.is_available(&self.data_dir))
            .collect()
    }

    /// Get the baseline reranker for bake-off comparison.
    pub fn baseline_reranker(&self) -> Option<&'static RegisteredReranker> {
        RERANKERS.iter().find(|r| r.is_baseline)
    }

    /// Validate that a reranker is ready to use.
    ///
    /// Returns `Ok(())` if available, or an error with details about what's missing.
    pub fn validate(&self, name: &str) -> RerankerResult<&'static RegisteredReranker> {
        let reranker = self.get(name).ok_or_else(|| {
            RerankerError::Unavailable(format!(
                "unknown reranker '{}'. Available: {}",
                name,
                RERANKERS
                    .iter()
                    .map(|r| r.name)
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;

        if !reranker.is_available(&self.data_dir) {
            let missing = reranker.missing_files(&self.data_dir);
            let model_dir = reranker
                .model_dir(&self.data_dir)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "unknown".to_string());

            return Err(RerankerError::Unavailable(format!(
                "reranker '{}' not available: missing files in {}: {}. Run 'cass models install' to download.",
                name,
                model_dir,
                missing.join(", ")
            )));
        }

        Ok(reranker)
    }
}

/// Load a reranker by name (or default if None).
///
/// # Arguments
///
/// * `data_dir` - The cass data directory containing model files.
/// * `name` - Optional reranker name. If None, uses the best available.
///
/// # Returns
///
/// An `Arc<dyn Reranker>` ready for use, or an error if unavailable.
pub fn get_reranker(data_dir: &Path, name: Option<&str>) -> RerankerResult<Arc<dyn Reranker>> {
    let registry = RerankerRegistry::new(data_dir);

    let reranker_info = match name {
        Some(n) => registry.validate(n)?,
        None => registry
            .best_available()
            .ok_or_else(|| RerankerError::Unavailable("no rerankers available".to_string()))?,
    };

    load_reranker_by_name(data_dir, reranker_info.name)
}

/// Load a reranker by registered name.
fn load_reranker_by_name(data_dir: &Path, name: &str) -> RerankerResult<Arc<dyn Reranker>> {
    match name {
        // All ONNX-based rerankers (baseline and bake-off candidates)
        "ms-marco" | "bge-reranker-v2" | "jina-reranker-turbo" | "jina-reranker-v2" => {
            let model_dir = RERANKERS
                .iter()
                .find(|r| r.name == name)
                .and_then(|r| r.model_dir(data_dir))
                .ok_or_else(|| {
                    RerankerError::Unavailable(format!("no model dir for reranker: {}", name))
                })?;
            let reranker = FastEmbedReranker::load_from_dir(&model_dir)?;
            Ok(Arc::new(reranker))
        }
        _ => Err(RerankerError::Unavailable(format!(
            "reranker '{}' not implemented",
            name
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_registry_all() {
        let tmp = tempdir().unwrap();
        let registry = RerankerRegistry::new(tmp.path());
        assert!(registry.all().len() >= 4);
    }

    #[test]
    fn test_registry_get_by_name() {
        let tmp = tempdir().unwrap();
        let registry = RerankerRegistry::new(tmp.path());

        let msmarco = registry.get("ms-marco");
        assert!(msmarco.is_some());
        assert_eq!(msmarco.unwrap().id, "ms-marco-minilm-l6-v2");

        let bge = registry.get("bge-reranker-v2");
        assert!(bge.is_some());
        assert_eq!(bge.unwrap().id, "bge-reranker-v2-m3");

        let unknown = registry.get("unknown");
        assert!(unknown.is_none());
    }

    #[test]
    fn test_registry_get_by_id() {
        let tmp = tempdir().unwrap();
        let registry = RerankerRegistry::new(tmp.path());

        let msmarco = registry.get("ms-marco-minilm-l6-v2");
        assert!(msmarco.is_some());
        assert_eq!(msmarco.unwrap().name, "ms-marco");
    }

    #[test]
    fn test_rerankers_unavailable_without_files() {
        let tmp = tempdir().unwrap();
        let registry = RerankerRegistry::new(tmp.path());

        // All rerankers should be unavailable without model files
        for r in registry.all() {
            assert!(
                !registry.is_available(r.name),
                "{} should be unavailable without files",
                r.name
            );
        }
    }

    #[test]
    fn test_best_available_none() {
        let tmp = tempdir().unwrap();
        let registry = RerankerRegistry::new(tmp.path());

        // Without model files, best_available should return None
        let best = registry.best_available();
        assert!(best.is_none());
    }

    #[test]
    fn test_validate_unknown_reranker() {
        let tmp = tempdir().unwrap();
        let registry = RerankerRegistry::new(tmp.path());

        let result = registry.validate("nonexistent");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("unknown reranker"));
        assert!(err.to_string().contains("Available:"));
    }

    #[test]
    fn test_registered_reranker_missing_files() {
        let tmp = tempdir().unwrap();
        let registry = RerankerRegistry::new(tmp.path());

        let msmarco = registry.get("ms-marco").unwrap();
        let missing = msmarco.missing_files(tmp.path());
        assert!(!missing.is_empty());
        assert!(missing.contains(&"model.onnx".to_string()));
    }

    // ==================== Bake-off Tests ====================

    #[test]
    fn test_bakeoff_eligible_count() {
        let tmp = tempdir().unwrap();
        let registry = RerankerRegistry::new(tmp.path());

        let eligible = registry.bakeoff_eligible();
        // Should have at least 3 eligible models (bge-v2, jina-turbo, jina-v2)
        assert!(
            eligible.len() >= 3,
            "Expected at least 3 eligible rerankers, got {}",
            eligible.len()
        );

        // ms-marco should NOT be in the eligible list (it's the baseline)
        assert!(
            !eligible.iter().any(|r| r.name == "ms-marco"),
            "ms-marco should not be in eligible list"
        );
    }

    #[test]
    fn test_baseline_reranker() {
        let tmp = tempdir().unwrap();
        let registry = RerankerRegistry::new(tmp.path());

        let baseline = registry.baseline_reranker();
        assert!(baseline.is_some());
        let baseline = baseline.unwrap();
        assert_eq!(baseline.name, "ms-marco");
        assert!(baseline.is_baseline);
        assert!(!baseline.is_bakeoff_eligible());
    }

    #[test]
    fn test_bakeoff_eligibility_by_date() {
        let tmp = tempdir().unwrap();
        let registry = RerankerRegistry::new(tmp.path());

        // ms-marco was released before cutoff (2022-01-01)
        let msmarco = registry.get("ms-marco").unwrap();
        assert!(
            msmarco.release_date < BAKEOFF_ELIGIBILITY_CUTOFF,
            "ms-marco should be released before cutoff"
        );

        // All eligible models should be released after cutoff
        for r in registry.bakeoff_eligible() {
            assert!(
                r.release_date >= BAKEOFF_ELIGIBILITY_CUTOFF,
                "{} should be released after cutoff (date: {})",
                r.name,
                r.release_date
            );
        }
    }

    #[test]
    fn test_bakeoff_model_metadata_conversion() {
        let tmp = tempdir().unwrap();
        let registry = RerankerRegistry::new(tmp.path());

        let msmarco = registry.get("ms-marco").unwrap();
        let metadata = msmarco.to_model_metadata();

        assert_eq!(metadata.id, "ms-marco-minilm-l6-v2");
        assert_eq!(metadata.name, "ms-marco");
        assert!(metadata.source.contains("ms-marco"));
        assert_eq!(metadata.release_date, "2022-01-01");
        assert!(metadata.dimension.is_none()); // Rerankers don't have dimensions
        assert!(metadata.is_baseline);
        assert!(!metadata.is_eligible());
    }

    #[test]
    fn test_eligible_reranker_metadata() {
        let tmp = tempdir().unwrap();
        let registry = RerankerRegistry::new(tmp.path());

        // Check BGE reranker
        let bge = registry.get("bge-reranker-v2").unwrap();
        assert!(bge.is_bakeoff_eligible());
        let metadata = bge.to_model_metadata();
        assert!(!metadata.is_baseline);
        assert!(metadata.is_eligible());

        // Check Jina reranker
        let jina = registry.get("jina-reranker-turbo").unwrap();
        assert!(jina.is_bakeoff_eligible());
    }

    #[test]
    fn test_all_rerankers_have_required_fields() {
        for r in RERANKERS.iter() {
            // All should have valid release dates
            assert!(
                !r.release_date.is_empty(),
                "{} should have a release date",
                r.name
            );

            // All should have HuggingFace IDs
            if r.requires_model_files {
                assert!(
                    !r.huggingface_id.is_empty(),
                    "{} should have a huggingface_id",
                    r.name
                );
            }
        }
    }

    #[test]
    fn test_model_dir_for_all_rerankers() {
        let tmp = tempdir().unwrap();

        for r in RERANKERS.iter() {
            if r.requires_model_files {
                let dir = r.model_dir(tmp.path());
                assert!(dir.is_some(), "{} should have a model directory", r.name);
                let dir = dir.unwrap();
                assert!(
                    dir.starts_with(tmp.path().join("models")),
                    "{} model dir should be under models/",
                    r.name
                );
            }
        }
    }
}
