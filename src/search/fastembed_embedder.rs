//! FastEmbed-based ML embedders.
//!
//! Loads local ONNX model + tokenizer bundles and produces semantic embeddings.
//! This implementation never downloads model assets; it expects the model files
//! to be present on disk and returns a clear error when they are missing.
//!
//! Supports multiple models:
//! - MiniLM (baseline)
//! - EmbeddingGemma (bake-off candidate)
//! - Qwen3-Embedding (bake-off candidate)
//! - ModernBERT-embed (bake-off candidate)
//! - Snowflake Arctic Embed (bake-off candidate)
//! - Nomic Embed Text (bake-off candidate)
//!
//! # `semantic` feature gate (cass#256)
//!
//! When the `semantic` Cargo feature is **disabled** (i.e. baseline build), the
//! `fastembed` crate and the prebuilt Microsoft ONNX Runtime binary it pulls in
//! are not linked. In that build the loader methods (`load_from_dir`,
//! `load_with_config`, `load_by_name`) return a stable
//! `EmbedderError::EmbedderUnavailable` describing the missing capability. The
//! free static methods (`canonical_name`, `model_dir_for`, `embedder_id_static`,
//! `default_model_dir`, `config_for`, `select_model_file`,
//! `runtime_model_dir_for`, etc.) remain fully functional so the lexical-only
//! search path continues to compile and run.

use std::path::{Path, PathBuf};

#[cfg(feature = "semantic")]
use std::fs;
#[cfg(feature = "semantic")]
use std::sync::Mutex;

#[cfg(feature = "semantic")]
use fastembed::{
    InitOptionsUserDefined, Pooling, TextEmbedding, TokenizerFiles, UserDefinedEmbeddingModel,
};

use super::embedder::{Embedder, EmbedderError, EmbedderResult};
use frankensearch::{ModelCategory, ModelTier};

/// Stand-in for `fastembed::Pooling` when the `semantic` feature is disabled.
///
/// Mirrors only the variants that cass references (`Mean` is the sole pooling
/// strategy in [`OnnxEmbedderConfig::default`]). The variant carries no
/// behaviour in baseline builds because all loader paths return
/// `EmbedderUnavailable` before pooling would be consulted.
#[cfg(not(feature = "semantic"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pooling {
    Mean,
}

// MiniLM constants (baseline)
const MINILM_MODEL_ID: &str = "all-minilm-l6-v2";
const MINILM_DIR_NAME: &str = "all-MiniLM-L6-v2";
const MINILM_EMBEDDER_ID: &str = "minilm-384";
const MINILM_DIMENSION: usize = 384;

// Standard ONNX file names: prefer onnx/ subdir (modern layout), fall back to flat (legacy).
pub const MODEL_ONNX_SUBDIR: &str = "onnx/model.onnx";
pub const MODEL_ONNX_LEGACY: &str = "model.onnx";
const TOKENIZER_JSON: &str = "tokenizer.json";
const CONFIG_JSON: &str = "config.json";
const SPECIAL_TOKENS_JSON: &str = "special_tokens_map.json";
const TOKENIZER_CONFIG_JSON: &str = "tokenizer_config.json";

/// Configuration for loading an ONNX embedder.
#[derive(Debug, Clone)]
pub struct OnnxEmbedderConfig {
    /// Unique embedder ID (e.g., "minilm-384").
    pub embedder_id: String,
    /// Model identifier for logging.
    pub model_id: String,
    /// Output embedding dimension.
    pub dimension: usize,
    /// Pooling strategy.
    pub pooling: Pooling,
}

impl Default for OnnxEmbedderConfig {
    fn default() -> Self {
        Self {
            embedder_id: MINILM_EMBEDDER_ID.to_string(),
            model_id: MINILM_MODEL_ID.to_string(),
            dimension: MINILM_DIMENSION,
            pooling: Pooling::Mean,
        }
    }
}

/// FastEmbed-backed semantic embedder.
///
/// Supports multiple ONNX models with configurable dimensions and pooling.
///
/// In baseline builds (`#[cfg(not(feature = "semantic"))]`), the `model`
/// field is omitted and any loader method returns
/// `EmbedderError::EmbedderUnavailable`.
pub struct FastEmbedder {
    #[cfg(feature = "semantic")]
    model: Mutex<TextEmbedding>,
    id: String,
    model_id: String,
    dimension: usize,
}

impl FastEmbedder {
    /// Stable embedder identifier for MiniLM (matches vector index naming).
    pub fn embedder_id_static() -> &'static str {
        MINILM_EMBEDDER_ID
    }

    /// Stable model identifier for MiniLM.
    pub fn model_id_static() -> &'static str {
        MINILM_MODEL_ID
    }

    /// Required non-model files for any ONNX embedder.
    ///
    /// The ONNX model itself can live at `onnx/model.onnx` (modern) or
    /// `model.onnx` (legacy); use [`select_model_file`] to find it.
    pub fn required_model_files() -> &'static [&'static str] {
        &[
            TOKENIZER_JSON,
            CONFIG_JSON,
            SPECIAL_TOKENS_JSON,
            TOKENIZER_CONFIG_JSON,
        ]
    }

    /// Candidate ONNX model locations, ordered from preferred to legacy.
    pub fn model_file_candidates() -> &'static [&'static str] {
        &[MODEL_ONNX_SUBDIR, MODEL_ONNX_LEGACY]
    }

    /// Select the ONNX model file, preferring `onnx/model.onnx` over `model.onnx`.
    pub fn select_model_file(model_dir: &Path) -> Option<PathBuf> {
        for candidate in Self::model_file_candidates() {
            let path = model_dir.join(candidate);
            if path.is_file() {
                return Some(path);
            }
        }
        None
    }

    /// Default MiniLM model directory relative to the cass data dir.
    pub fn default_model_dir(data_dir: &Path) -> PathBuf {
        data_dir.join("models").join(MINILM_DIR_NAME)
    }

    /// Get model directory for a specific embedder name.
    pub fn model_dir_for(data_dir: &Path, embedder_name: &str) -> Option<PathBuf> {
        let dir_name = match Self::canonical_name(embedder_name)? {
            "minilm" => MINILM_DIR_NAME,
            "snowflake-arctic-s" => "snowflake-arctic-embed-s",
            "nomic-embed" => "nomic-embed-text-v1.5",
            _ => return None,
        };
        Some(data_dir.join("models").join(dir_name))
    }

    /// Resolve the runtime model directory for an embedder.
    ///
    /// `model_dir_for` is the cass-managed cache location. This variant honors
    /// the explicit FRANKENSEARCH_MODEL_DIR override used by operators who
    /// pre-stage a model bundle outside the cass data directory.
    pub fn runtime_model_dir_for(data_dir: &Path, embedder_name: &str) -> Option<PathBuf> {
        model_dir_override().or_else(|| Self::model_dir_for(data_dir, embedder_name))
    }

    pub fn canonical_name(embedder_name: &str) -> Option<&'static str> {
        match embedder_name.trim().to_ascii_lowercase().as_str() {
            "fastembed" | "minilm" | "all-minilm-l6-v2" | "minilm-384" => Some("minilm"),
            "snowflake"
            | "snowflake-arctic-s"
            | "snowflake-arctic-embed-s"
            | "snowflake-arctic-s-384" => Some("snowflake-arctic-s"),
            "nomic" | "nomic-embed" | "nomic-embed-text-v1.5" | "nomic-embed-768" => {
                Some("nomic-embed")
            }
            _ => None,
        }
    }

    /// Get config for a specific embedder by name.
    pub fn config_for(embedder_name: &str) -> Option<OnnxEmbedderConfig> {
        match Self::canonical_name(embedder_name)? {
            "minilm" => Some(OnnxEmbedderConfig {
                embedder_id: "minilm-384".to_string(),
                model_id: "all-minilm-l6-v2".to_string(),
                dimension: 384,
                pooling: Pooling::Mean,
            }),
            "snowflake-arctic-s" => Some(OnnxEmbedderConfig {
                embedder_id: "snowflake-arctic-s-384".to_string(),
                model_id: "snowflake-arctic-embed-s".to_string(),
                dimension: 384,
                pooling: Pooling::Mean,
            }),
            "nomic-embed" => Some(OnnxEmbedderConfig {
                embedder_id: "nomic-embed-768".to_string(),
                model_id: "nomic-embed-text-v1.5".to_string(),
                dimension: 768,
                pooling: Pooling::Mean,
            }),
            _ => None,
        }
    }

    /// Load the MiniLM model (convenience wrapper).
    ///
    /// Baseline builds (no `semantic` feature) return
    /// `EmbedderError::EmbedderUnavailable` because the prebuilt ONNX Runtime
    /// is not linked; see the crate-level note on cass#256.
    #[cfg(feature = "semantic")]
    pub fn load_from_dir(model_dir: &Path) -> EmbedderResult<Self> {
        Self::load_with_config(model_dir, OnnxEmbedderConfig::default())
    }

    #[cfg(not(feature = "semantic"))]
    pub fn load_from_dir(_model_dir: &Path) -> EmbedderResult<Self> {
        Err(Self::unavailable_error(
            MINILM_EMBEDDER_ID,
            "semantic search is not available in this build (cass was built without the `semantic` feature; rebuild with `--features semantic` or use the full release artifact)",
        ))
    }

    /// Load an ONNX embedder with custom configuration.
    #[cfg(feature = "semantic")]
    pub fn load_with_config(model_dir: &Path, config: OnnxEmbedderConfig) -> EmbedderResult<Self> {
        if !model_dir.is_dir() {
            return Err(Self::unavailable_error(
                &config.embedder_id,
                format!("model directory not found: {}", model_dir.display()),
            ));
        }

        let onnx_path = Self::select_model_file(model_dir).ok_or_else(|| {
            Self::unavailable_error(
                &config.embedder_id,
                format!(
                    "no ONNX model file in {} (checked {} and {})",
                    model_dir.display(),
                    MODEL_ONNX_SUBDIR,
                    MODEL_ONNX_LEGACY
                ),
            )
        })?;

        let required = Self::required_model_files();
        let mut missing = Vec::new();
        for name in required {
            let path = model_dir.join(name);
            if !path.is_file() {
                missing.push(*name);
            }
        }
        if !missing.is_empty() {
            return Err(Self::unavailable_error(
                &config.embedder_id,
                format!(
                    "model files missing in {}: {}",
                    model_dir.display(),
                    missing.join(", ")
                ),
            ));
        }

        let model_file = Self::read_required(onnx_path, "model.onnx", &config.embedder_id)?;
        let tokenizer_file = Self::read_required(
            model_dir.join(TOKENIZER_JSON),
            TOKENIZER_JSON,
            &config.embedder_id,
        )?;
        let config_file = Self::read_required(
            model_dir.join(CONFIG_JSON),
            CONFIG_JSON,
            &config.embedder_id,
        )?;
        let special_tokens_map_file = Self::read_required(
            model_dir.join(SPECIAL_TOKENS_JSON),
            SPECIAL_TOKENS_JSON,
            &config.embedder_id,
        )?;
        let tokenizer_config_file = Self::read_required(
            model_dir.join(TOKENIZER_CONFIG_JSON),
            TOKENIZER_CONFIG_JSON,
            &config.embedder_id,
        )?;

        let tokenizer_files = TokenizerFiles {
            tokenizer_file,
            config_file,
            special_tokens_map_file,
            tokenizer_config_file,
        };

        let mut model = UserDefinedEmbeddingModel::new(model_file, tokenizer_files);
        model.pooling = Some(config.pooling);

        let init_options = InitOptionsUserDefined::new();

        let model = TextEmbedding::try_new_from_user_defined(model, init_options).map_err(|e| {
            EmbedderError::EmbeddingFailed {
                model: config.embedder_id.clone(),
                source: Box::new(std::io::Error::other(format!("fastembed init failed: {e}"))),
            }
        })?;

        Ok(Self {
            model: Mutex::new(model),
            id: config.embedder_id,
            model_id: config.model_id,
            dimension: config.dimension,
        })
    }

    /// Baseline-build stub: see the crate-level note on cass#256.
    #[cfg(not(feature = "semantic"))]
    pub fn load_with_config(_model_dir: &Path, config: OnnxEmbedderConfig) -> EmbedderResult<Self> {
        Err(Self::unavailable_error(
            &config.embedder_id,
            "semantic search is not available in this build (cass was built without the `semantic` feature; rebuild with `--features semantic` or use the full release artifact)",
        ))
    }

    /// Load an embedder by name from the data directory.
    #[cfg(feature = "semantic")]
    pub fn load_by_name(data_dir: &Path, embedder_name: &str) -> EmbedderResult<Self> {
        let canonical_name = Self::canonical_name(embedder_name).ok_or_else(|| {
            Self::unavailable_error(
                embedder_name,
                format!("unknown embedder: {}", embedder_name),
            )
        })?;
        let model_dir = Self::runtime_model_dir_for(data_dir, canonical_name).ok_or_else(|| {
            Self::unavailable_error(
                embedder_name,
                format!("unknown embedder: {}", embedder_name),
            )
        })?;
        let config = Self::config_for(canonical_name).ok_or_else(|| {
            Self::unavailable_error(
                embedder_name,
                format!("no config for embedder: {}", embedder_name),
            )
        })?;
        Self::load_with_config(&model_dir, config)
    }

    #[cfg(not(feature = "semantic"))]
    pub fn load_by_name(_data_dir: &Path, embedder_name: &str) -> EmbedderResult<Self> {
        Err(Self::unavailable_error(
            embedder_name,
            "semantic search is not available in this build (cass was built without the `semantic` feature; rebuild with `--features semantic` or use the full release artifact)",
        ))
    }

    /// Stable model identifier for compatibility checks.
    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    #[cfg(feature = "semantic")]
    fn read_required(path: PathBuf, label: &str, model_id: &str) -> EmbedderResult<Vec<u8>> {
        fs::read(&path).map_err(|e| {
            Self::unavailable_error(
                model_id,
                format!("unable to read {label} at {}: {e}", path.display()),
            )
        })
    }

    fn unavailable_error(model: impl Into<String>, reason: impl Into<String>) -> EmbedderError {
        EmbedderError::EmbedderUnavailable {
            model: model.into(),
            reason: reason.into(),
        }
    }

    #[cfg(feature = "semantic")]
    fn normalize_in_place(embedding: &mut [f32]) {
        let norm_sq: f32 = embedding.iter().map(|x| x * x).sum();
        if norm_sq.is_finite() && norm_sq > f32::EPSILON {
            let inv_norm = 1.0 / norm_sq.sqrt();
            for v in embedding.iter_mut() {
                *v *= inv_norm;
            }
        } else {
            // NaN/Inf contamination: zero out to prevent poisoning similarity search.
            embedding.fill(0.0);
        }
    }
}

pub fn model_dir_override() -> Option<PathBuf> {
    dotenvy::var("FRANKENSEARCH_MODEL_DIR")
        .ok()
        .map(|raw| raw.trim().to_string())
        .filter(|raw| !raw.is_empty())
        .map(|raw| expand_model_dir_override(&raw))
}

fn expand_model_dir_override(raw: &str) -> PathBuf {
    if raw == "~" {
        return dotenvy::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(raw));
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return dotenvy::var("HOME")
            .map(|home| PathBuf::from(home).join(rest))
            .unwrap_or_else(|_| PathBuf::from(raw));
    }
    PathBuf::from(raw)
}

#[cfg(feature = "semantic")]
impl Embedder for FastEmbedder {
    fn embed_sync(&self, text: &str) -> EmbedderResult<Vec<f32>> {
        if text.is_empty() {
            return Err(EmbedderError::InvalidConfig {
                field: "input_text".to_string(),
                value: "(empty)".to_string(),
                reason: "empty text".to_string(),
            });
        }

        #[allow(unused_mut)]
        let mut model = self
            .model
            .lock()
            .map_err(|_| EmbedderError::SubsystemError {
                subsystem: "embedder",
                source: Box::new(std::io::Error::other("fastembed lock poisoned")),
            })?;

        let embeddings =
            model
                .embed(vec![text], None)
                .map_err(|e| EmbedderError::EmbeddingFailed {
                    model: self.id.clone(),
                    source: Box::new(std::io::Error::other(format!(
                        "fastembed embed failed: {e}"
                    ))),
                })?;

        let mut embedding =
            embeddings
                .into_iter()
                .next()
                .ok_or_else(|| EmbedderError::EmbeddingFailed {
                    model: self.id.clone(),
                    source: Box::new(std::io::Error::other("fastembed returned no embedding")),
                })?;

        if embedding.len() != self.dimension {
            return Err(EmbedderError::EmbeddingFailed {
                model: self.id.clone(),
                source: Box::new(std::io::Error::other(format!(
                    "fastembed dimension mismatch: expected {}, got {}",
                    self.dimension,
                    embedding.len()
                ))),
            });
        }

        Self::normalize_in_place(&mut embedding);
        Ok(embedding)
    }

    fn embed_batch_sync(&self, texts: &[&str]) -> EmbedderResult<Vec<Vec<f32>>> {
        for text in texts {
            if text.is_empty() {
                return Err(EmbedderError::InvalidConfig {
                    field: "input_text".to_string(),
                    value: "(empty)".to_string(),
                    reason: "empty text in batch".to_string(),
                });
            }
        }

        if texts.is_empty() {
            return Ok(Vec::new());
        }

        #[allow(unused_mut)]
        let mut model = self
            .model
            .lock()
            .map_err(|_| EmbedderError::SubsystemError {
                subsystem: "embedder",
                source: Box::new(std::io::Error::other("fastembed lock poisoned")),
            })?;

        let inputs = texts.to_vec();
        let mut embeddings =
            model
                .embed(inputs, None)
                .map_err(|e| EmbedderError::EmbeddingFailed {
                    model: self.id.clone(),
                    source: Box::new(std::io::Error::other(format!(
                        "fastembed embed failed: {e}"
                    ))),
                })?;

        for embedding in embeddings.iter_mut() {
            if embedding.len() != self.dimension {
                return Err(EmbedderError::EmbeddingFailed {
                    model: self.id.clone(),
                    source: Box::new(std::io::Error::other(format!(
                        "fastembed dimension mismatch: expected {}, got {}",
                        self.dimension,
                        embedding.len()
                    ))),
                });
            }
            Self::normalize_in_place(embedding);
        }

        Ok(embeddings)
    }

    fn dimension(&self) -> usize {
        self.dimension
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn model_name(&self) -> &str {
        &self.model_id
    }

    fn is_semantic(&self) -> bool {
        true
    }

    fn category(&self) -> ModelCategory {
        ModelCategory::TransformerEmbedder
    }

    fn tier(&self) -> ModelTier {
        ModelTier::Quality
    }
}

// Baseline-build `Embedder` impl. `FastEmbedder` cannot actually be
// instantiated in this build (`load_*` all return `EmbedderUnavailable`),
// so the `embed_sync` / `embed_batch_sync` arms are unreachable in practice.
// We still provide the impl so existing `Arc<dyn Embedder>` plumbing
// compiles without a `cfg`-shower in every call site.
#[cfg(not(feature = "semantic"))]
impl Embedder for FastEmbedder {
    fn embed_sync(&self, _text: &str) -> EmbedderResult<Vec<f32>> {
        Err(Self::unavailable_error(
            &self.id,
            "semantic search is not available in this build (cass was built without the `semantic` feature; rebuild with `--features semantic` or use the full release artifact)",
        ))
    }

    fn embed_batch_sync(&self, _texts: &[&str]) -> EmbedderResult<Vec<Vec<f32>>> {
        Err(Self::unavailable_error(
            &self.id,
            "semantic search is not available in this build (cass was built without the `semantic` feature; rebuild with `--features semantic` or use the full release artifact)",
        ))
    }

    fn dimension(&self) -> usize {
        self.dimension
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn model_name(&self) -> &str {
        &self.model_id
    }

    fn is_semantic(&self) -> bool {
        true
    }

    fn category(&self) -> ModelCategory {
        ModelCategory::TransformerEmbedder
    }

    fn tier(&self) -> ModelTier {
        ModelTier::Quality
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn fastembed_missing_files_returns_unavailable() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let err = FastEmbedder::load_from_dir(tmp.path())
            .err()
            .expect("missing model should fail");
        assert!(
            matches!(err, EmbedderError::EmbedderUnavailable { .. }),
            "expected EmbedderUnavailable, got {err:?}"
        );
    }

    #[test]
    fn unavailable_error_preserves_shape() {
        let err = FastEmbedder::unavailable_error("test-model", "missing files");
        assert!(std::error::Error::source(&err).is_none());
        match err {
            EmbedderError::EmbedderUnavailable { model, reason } => {
                assert_eq!(model, "test-model");
                assert_eq!(reason, "missing files");
            }
            other => panic!("expected EmbedderUnavailable, got {other:?}"),
        }
    }

    #[test]
    fn select_model_file_prefers_modern_onnx_layout() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join("onnx")).unwrap();
        std::fs::write(tmp.path().join("onnx/model.onnx"), b"modern").unwrap();
        std::fs::write(tmp.path().join("model.onnx"), b"legacy").unwrap();

        let selected = FastEmbedder::select_model_file(tmp.path()).unwrap();
        assert!(
            selected.ends_with("onnx/model.onnx"),
            "should prefer onnx/ subdir: {selected:?}"
        );
    }

    #[test]
    fn select_model_file_falls_back_to_legacy() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("model.onnx"), b"legacy").unwrap();

        let selected = FastEmbedder::select_model_file(tmp.path()).unwrap();
        assert!(
            selected.ends_with("model.onnx"),
            "should fall back to legacy: {selected:?}"
        );
    }

    #[test]
    fn select_model_file_returns_none_for_empty_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(FastEmbedder::select_model_file(tmp.path()).is_none());
    }

    #[test]
    fn config_for_known_models() {
        let minilm = FastEmbedder::config_for("minilm").unwrap();
        assert_eq!(minilm.dimension, 384);

        let snowflake = FastEmbedder::config_for("snowflake-arctic-s").unwrap();
        assert_eq!(snowflake.dimension, 384);

        let nomic = FastEmbedder::config_for("nomic-embed").unwrap();
        assert_eq!(nomic.dimension, 768);

        assert!(FastEmbedder::config_for("unknown").is_none());
    }

    #[test]
    fn canonical_name_accepts_policy_and_index_aliases() {
        assert_eq!(FastEmbedder::canonical_name("fastembed"), Some("minilm"));
        assert_eq!(
            FastEmbedder::canonical_name("snowflake-arctic-s-384"),
            Some("snowflake-arctic-s")
        );
        assert_eq!(
            FastEmbedder::canonical_name("nomic-embed-text-v1.5"),
            Some("nomic-embed")
        );
    }

    #[test]
    #[serial]
    fn runtime_model_dir_honors_frankensearch_override_and_expands_home() {
        let old_override = dotenvy::var("FRANKENSEARCH_MODEL_DIR").ok();
        let old_home = dotenvy::var("HOME").ok();
        unsafe {
            std::env::set_var("HOME", "/tmp/cass-home-for-model-test");
            std::env::set_var("FRANKENSEARCH_MODEL_DIR", "~/models/snowflake");
        }

        let resolved = FastEmbedder::runtime_model_dir_for(Path::new("/tmp/cass"), "snowflake")
            .expect("runtime model dir");
        assert_eq!(
            resolved,
            PathBuf::from("/tmp/cass-home-for-model-test/models/snowflake")
        );

        unsafe {
            if let Some(value) = old_override {
                std::env::set_var("FRANKENSEARCH_MODEL_DIR", value);
            } else {
                std::env::remove_var("FRANKENSEARCH_MODEL_DIR");
            }
            if let Some(value) = old_home {
                std::env::set_var("HOME", value);
            } else {
                std::env::remove_var("HOME");
            }
        }
    }
}
