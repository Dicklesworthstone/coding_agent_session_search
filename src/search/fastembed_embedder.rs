//! Pure-Rust ML embedder.
//!
//! Loads a local safetensors model + tokenizer bundle and produces semantic
//! embeddings via frankensearch's [`NativeEmbedder`](frankensearch::NativeEmbedder)
//! — a pure-Rust (frankentorch) sentence embedder with **no ONNX Runtime / no
//! `ort`**. The default is `all-MiniLM-L6-v2`; the distinct, opt-in
//! `paraphrase-multilingual-MiniLM-L12-v2` space supports CJK and mixed-language
//! archives. This implementation never downloads model assets; it expects the
//! model files to be present on disk and returns a clear error when they are
//! missing.
//!
//! The type is still named `FastEmbedder` for call-site stability (the registry,
//! model management, and vector-index naming reference it), but the FastEmbed /
//! ONNX backend was removed in cass #308: `ort 2.0.0-rc.12` could not run the
//! `all-MiniLM-L6-v2` `LayerNormalization` export, and its prebuilt AVX/AVX2
//! vendor binaries crashed pre-AVX2 CPUs at static init (#256/#307). The
//! pure-Rust backend has neither problem — no AVX-static-init hazard, so a single
//! binary runs everywhere (the `-baseline` artifact is no longer needed).
//!
//! Only the two exact, manifest-attested 384-dimensional MiniLM topologies are
//! supported. They deliberately use different embedder IDs and vector-space
//! revisions; sharing a dimension never makes their vectors interchangeable.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use super::embedder::{Embedder, EmbedderError, EmbedderResult};
use frankensearch::core::EmbeddingIdentityBundleV1;
use frankensearch::{ModelCategory, ModelTier, NativeEmbedder, NativeEmbeddingModel};

/// Pooling strategy for the embedder configuration. The native embedder always
/// mean-pools over every token (the sentence-transformers all-MiniLM head), so
/// `Mean` is the only meaningful variant; the enum is retained for the
/// [`OnnxEmbedderConfig`] API consumed across the search stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pooling {
    Mean,
}

// MiniLM constants (baseline)
const MINILM_MODEL_ID: &str = "all-minilm-l6-v2";
const MINILM_DIR_NAME: &str = "all-MiniLM-L6-v2";
const MINILM_EMBEDDER_ID: &str = "minilm-384";
const MULTILINGUAL_MINILM_MODEL_ID: &str = "paraphrase-multilingual-minilm-l12-v2";
const MULTILINGUAL_MINILM_DIR_NAME: &str = "paraphrase-multilingual-MiniLM-L12-v2";
const MULTILINGUAL_MINILM_EMBEDDER_ID: &str = "multilingual-minilm-384";
const MINILM_DIMENSION: usize = 384;

/// FSVI vector-space revision for the native MiniLM implementation.
///
/// The model revision alone is insufficient: pre-#308 ONNX and current native
/// inference can share an embedder ID and dimension without producing an
/// identical vector space. Persisting the engine generation prevents those
/// same-shape vectors from being mixed silently.
pub const MINILM_VECTOR_SPACE_REVISION: &str =
    "native-minilm-v1:c9745ed1d9f207416be6d2e6f8de32d1f16199bf";

/// FSVI vector-space revision for the opt-in multilingual native model.
///
/// The suffix is the frozen Frankensearch artifact/execution-manifest
/// fingerprint, not merely the shared 384-dimensional geometry.
pub const MULTILINGUAL_MINILM_VECTOR_SPACE_REVISION: &str = "native-multilingual-minilm-v1:59160d9e43d396d05b4139c99f9feb7922da14868587fca7e33d379821a41405";

// Safetensors model file names: prefer an explicit f32 export, fall back to the
// standard HuggingFace `model.safetensors`. The native embedder also needs
// `tokenizer.json`; `config.json` and the other tokenizer side-files are not
// consulted (the embedder reads the tokenizer + weights directly).
pub const MODEL_SAFETENSORS_PRIMARY: &str = "model_f32.safetensors";
pub const MODEL_SAFETENSORS: &str = "model.safetensors";
const TOKENIZER_JSON: &str = "tokenizer.json";

/// Configuration for loading a native embedder.
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

/// Pure-Rust semantic embedder (frankentorch `all-MiniLM-L6-v2`), wrapping
/// [`frankensearch::NativeEmbedder`]. Named `FastEmbedder` for call-site stability.
pub struct FastEmbedder {
    inner: NativeEmbedder,
    id: String,
    model_id: String,
    dimension: usize,
}

/// Metadata-stable, on-demand wrapper for the local quality embedder.
///
/// Semantic CLI searches normally prefer the resident daemon. Loading the
/// several-hundred-megabyte local model before the daemon is even probed adds
/// roughly eleven seconds to every short-lived process and defeats the daemon's
/// purpose (#347). This wrapper exposes the index-contract metadata eagerly but
/// initializes the local model only if daemon inference actually falls back.
pub struct LazyFastEmbedder {
    data_dir: PathBuf,
    canonical_name: String,
    config: OnnxEmbedderConfig,
    inner: OnceLock<Result<FastEmbedder, String>>,
}

impl LazyFastEmbedder {
    /// Construct a lazy wrapper for a known quality embedder.
    pub fn new(data_dir: &Path, embedder_name: &str) -> EmbedderResult<Self> {
        let canonical_name = FastEmbedder::canonical_name(embedder_name).ok_or_else(|| {
            FastEmbedder::unavailable_error(
                embedder_name,
                format!("unknown embedder: {embedder_name}"),
            )
        })?;
        let config = FastEmbedder::config_for(canonical_name).ok_or_else(|| {
            FastEmbedder::unavailable_error(
                embedder_name,
                format!("no config for embedder: {embedder_name}"),
            )
        })?;
        Ok(Self {
            data_dir: data_dir.to_path_buf(),
            canonical_name: canonical_name.to_string(),
            config,
            inner: OnceLock::new(),
        })
    }

    fn loaded(&self) -> EmbedderResult<&FastEmbedder> {
        match self.inner.get_or_init(|| {
            FastEmbedder::load_by_name(&self.data_dir, &self.canonical_name)
                .map_err(|err| err.to_string())
        }) {
            Ok(embedder) => Ok(embedder),
            Err(reason) => Err(FastEmbedder::unavailable_error(
                &self.config.embedder_id,
                reason.clone(),
            )),
        }
    }
}

impl Embedder for LazyFastEmbedder {
    fn embed_sync(&self, text: &str) -> EmbedderResult<Vec<f32>> {
        self.loaded()?.embed_sync(text)
    }

    fn embed_batch_sync(&self, texts: &[&str]) -> EmbedderResult<Vec<Vec<f32>>> {
        self.loaded()?.embed_batch_sync(texts)
    }

    fn dimension(&self) -> usize {
        self.config.dimension
    }

    fn identity(&self) -> EmbedderResult<&EmbeddingIdentityBundleV1> {
        self.loaded()?.identity()
    }

    fn id(&self) -> &str {
        &self.config.embedder_id
    }

    fn model_name(&self) -> &str {
        &self.config.model_id
    }

    fn is_ready(&self) -> bool {
        self.inner.get().is_none_or(Result::is_ok)
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

impl FastEmbedder {
    /// Stable embedder identifier for MiniLM (matches vector index naming).
    pub fn embedder_id_static() -> &'static str {
        MINILM_EMBEDDER_ID
    }

    /// Stable model identifier for MiniLM.
    pub fn model_id_static() -> &'static str {
        MINILM_MODEL_ID
    }

    /// Required non-model files for the native embedder. The safetensors weight
    /// file is located separately via [`select_model_file`].
    pub fn required_model_files() -> &'static [&'static str] {
        &[TOKENIZER_JSON]
    }

    /// Candidate safetensors weight locations, ordered from preferred to standard.
    pub fn model_file_candidates() -> &'static [&'static str] {
        &[MODEL_SAFETENSORS_PRIMARY, MODEL_SAFETENSORS]
    }

    /// Select the safetensors weight file, preferring `model_f32.safetensors`.
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
            "multilingual-minilm" => MULTILINGUAL_MINILM_DIR_NAME,
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
            "multilingual"
            | "multilingual-minilm"
            | "multilingual-minilm-384"
            | "paraphrase-multilingual-minilm-l12-v2" => Some("multilingual-minilm"),
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
            "multilingual-minilm" => Some(OnnxEmbedderConfig {
                embedder_id: MULTILINGUAL_MINILM_EMBEDDER_ID.to_string(),
                model_id: MULTILINGUAL_MINILM_MODEL_ID.to_string(),
                dimension: MINILM_DIMENSION,
                pooling: Pooling::Mean,
            }),
            _ => None,
        }
    }

    /// Load the MiniLM model (convenience wrapper).
    pub fn load_from_dir(model_dir: &Path) -> EmbedderResult<Self> {
        Self::load_with_config(model_dir, OnnxEmbedderConfig::default())
    }

    /// Load a native embedder with custom configuration.
    ///
    /// Only the exact manifest-attested MiniLM L6 and multilingual MiniLM L12
    /// profiles are supported; all other model IDs or dimensions are rejected.
    pub fn load_with_config(model_dir: &Path, config: OnnxEmbedderConfig) -> EmbedderResult<Self> {
        let profile = match (config.model_id.as_str(), config.dimension) {
            (MINILM_MODEL_ID, MINILM_DIMENSION) => NativeEmbeddingModel::AllMiniLmL6V2,
            (MULTILINGUAL_MINILM_MODEL_ID, MINILM_DIMENSION) => {
                NativeEmbeddingModel::ParaphraseMultilingualMiniLmL12V2
            }
            _ => {
                return Err(Self::unavailable_error(
                    &config.embedder_id,
                    format!(
                        "the pure-Rust native embedder supports only all-MiniLM-L6-v2 and \
                         paraphrase-multilingual-MiniLM-L12-v2 at 384 dimensions; {} ({}d) \
                         has no verified Frankentorch profile",
                        config.model_id, config.dimension
                    ),
                ));
            }
        };
        if !model_dir.is_dir() {
            return Err(Self::unavailable_error(
                &config.embedder_id,
                format!("model directory not found: {}", model_dir.display()),
            ));
        }
        if Self::select_model_file(model_dir).is_none() {
            return Err(Self::unavailable_error(
                &config.embedder_id,
                format!(
                    "no safetensors weight file in {} (checked {} and {})",
                    model_dir.display(),
                    MODEL_SAFETENSORS_PRIMARY,
                    MODEL_SAFETENSORS
                ),
            ));
        }
        if !model_dir.join(TOKENIZER_JSON).is_file() {
            return Err(Self::unavailable_error(
                &config.embedder_id,
                format!("missing {TOKENIZER_JSON} in {}", model_dir.display()),
            ));
        }

        let inner = NativeEmbedder::load_model(model_dir, profile)?;
        let dimension = inner.dimension();
        if dimension != config.dimension {
            return Err(Self::unavailable_error(
                &config.embedder_id,
                format!(
                    "native model output dimension {dimension} does not match the registered {}-d contract",
                    config.dimension
                ),
            ));
        }
        Ok(Self {
            inner,
            id: config.embedder_id,
            model_id: config.model_id,
            dimension,
        })
    }

    /// Load an embedder by name from the data directory.
    pub fn load_by_name(data_dir: &Path, embedder_name: &str) -> EmbedderResult<Self> {
        let canonical_name = Self::canonical_name(embedder_name).ok_or_else(|| {
            Self::unavailable_error(embedder_name, format!("unknown embedder: {embedder_name}"))
        })?;
        let model_dir = Self::runtime_model_dir_for(data_dir, canonical_name).ok_or_else(|| {
            Self::unavailable_error(embedder_name, format!("unknown embedder: {embedder_name}"))
        })?;
        let config = Self::config_for(canonical_name).ok_or_else(|| {
            Self::unavailable_error(
                embedder_name,
                format!("no config for embedder: {embedder_name}"),
            )
        })?;
        Self::load_with_config(&model_dir, config)
    }

    /// Stable model identifier for compatibility checks.
    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    fn unavailable_error(model: impl Into<String>, reason: impl Into<String>) -> EmbedderError {
        EmbedderError::EmbedderUnavailable {
            model: model.into(),
            reason: reason.into(),
        }
    }

    fn validate_output(&self, vectors: &[Vec<f32>], expected_count: usize) -> EmbedderResult<()> {
        if vectors.len() != expected_count {
            return Err(EmbedderError::EmbeddingFailed {
                model: self.model_id.clone(),
                source: Box::new(std::io::Error::other(format!(
                    "native embedder returned {} vectors for {expected_count} inputs",
                    vectors.len()
                ))),
            });
        }
        for (index, vector) in vectors.iter().enumerate() {
            if vector.len() != self.dimension {
                return Err(EmbedderError::EmbeddingFailed {
                    model: self.model_id.clone(),
                    source: Box::new(std::io::Error::other(format!(
                        "native embedding {index} has dimension {}, expected {}",
                        vector.len(),
                        self.dimension
                    ))),
                });
            }
            if vector.iter().any(|value| !value.is_finite()) {
                return Err(EmbedderError::EmbeddingFailed {
                    model: self.model_id.clone(),
                    source: Box::new(std::io::Error::other(format!(
                        "native embedding {index} contains a non-finite value"
                    ))),
                });
            }
        }
        Ok(())
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

impl Embedder for FastEmbedder {
    fn embed_sync(&self, text: &str) -> EmbedderResult<Vec<f32>> {
        if text.is_empty() {
            return Err(EmbedderError::InvalidConfig {
                field: "input_text".to_string(),
                value: "(empty)".to_string(),
                reason: "empty text".to_string(),
            });
        }
        let vector = self.inner.embed_sync(text)?;
        self.validate_output(std::slice::from_ref(&vector), 1)?;
        Ok(vector)
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
        let vectors = self.inner.embed_batch_sync(texts)?;
        self.validate_output(&vectors, texts.len())?;
        Ok(vectors)
    }

    fn dimension(&self) -> usize {
        self.dimension
    }

    fn identity(&self) -> EmbedderResult<&EmbeddingIdentityBundleV1> {
        self.inner.identity()
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

    #[test]
    fn missing_files_returns_unavailable() {
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
    fn issue_347_lazy_embedder_defers_model_initialization_until_fallback_inference() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let embedder = LazyFastEmbedder::new(tmp.path(), "minilm").expect("known model");

        assert_eq!(embedder.id(), "minilm-384");
        assert_eq!(embedder.dimension(), 384);
        assert_eq!(embedder.model_name(), "all-minilm-l6-v2");
        assert!(
            embedder.inner.get().is_none(),
            "construction must not load the local model"
        );

        let error = embedder
            .embed_sync("daemon fallback")
            .expect_err("missing local bundle must fail only when fallback is used");
        assert!(error.to_string().contains("model directory not found"));
        assert!(embedder.inner.get().is_some());
        assert!(!embedder.is_ready());
    }

    #[test]
    fn non_minilm_config_is_rejected_by_native_backend() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = OnnxEmbedderConfig {
            embedder_id: "nomic-embed-768".to_string(),
            model_id: "nomic-embed-text-v1.5".to_string(),
            dimension: 768,
            pooling: Pooling::Mean,
        };
        let err = FastEmbedder::load_with_config(tmp.path(), cfg)
            .err()
            .expect("768-dim model should be rejected");
        assert!(matches!(err, EmbedderError::EmbedderUnavailable { .. }));
    }

    #[test]
    fn config_for_only_native_supported_models() {
        assert_eq!(FastEmbedder::config_for("minilm").unwrap().dimension, 384);
        let multilingual = FastEmbedder::config_for("multilingual-minilm").unwrap();
        assert_eq!(multilingual.dimension, 384);
        assert_eq!(multilingual.embedder_id, MULTILINGUAL_MINILM_EMBEDDER_ID);
        assert!(FastEmbedder::config_for("snowflake-arctic-s").is_none());
        assert!(FastEmbedder::config_for("nomic-embed").is_none());
        assert!(FastEmbedder::config_for("unknown").is_none());
    }

    #[test]
    fn canonical_name_accepts_only_verified_native_aliases() {
        assert_eq!(FastEmbedder::canonical_name("fastembed"), Some("minilm"));
        assert_eq!(FastEmbedder::canonical_name("minilm-384"), Some("minilm"));
        assert_eq!(
            FastEmbedder::canonical_name("paraphrase-multilingual-minilm-l12-v2"),
            Some("multilingual-minilm")
        );
        assert_eq!(
            FastEmbedder::canonical_name("multilingual-minilm-384"),
            Some("multilingual-minilm")
        );
        assert!(FastEmbedder::canonical_name("snowflake-arctic-s-384").is_none());
        assert!(FastEmbedder::canonical_name("nomic-embed-text-v1.5").is_none());
    }

    #[test]
    fn same_dimension_models_have_distinct_index_contracts() {
        let baseline = FastEmbedder::config_for("minilm").unwrap();
        let multilingual = FastEmbedder::config_for("multilingual-minilm").unwrap();
        assert_eq!(baseline.dimension, multilingual.dimension);
        assert_ne!(baseline.embedder_id, multilingual.embedder_id);
        assert_ne!(
            MINILM_VECTOR_SPACE_REVISION,
            MULTILINGUAL_MINILM_VECTOR_SPACE_REVISION
        );
    }
}
