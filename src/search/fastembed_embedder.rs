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

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use fastembed::{
    InitOptionsUserDefined, Pooling, TextEmbedding, TokenizerFiles, UserDefinedEmbeddingModel,
};

use super::embedder::{Embedder, EmbedderError, EmbedderResult};

// MiniLM constants (baseline)
const MINILM_MODEL_ID: &str = "all-minilm-l6-v2";
const MINILM_DIR_NAME: &str = "all-MiniLM-L6-v2";
const MINILM_EMBEDDER_ID: &str = "minilm-384";
const MINILM_DIMENSION: usize = 384;

// Standard ONNX file names
const MODEL_FILE: &str = "model.onnx";
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
pub struct FastEmbedder {
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

    /// Required model files for any ONNX embedder.
    pub fn required_model_files() -> &'static [&'static str] {
        &[
            MODEL_FILE,
            TOKENIZER_JSON,
            CONFIG_JSON,
            SPECIAL_TOKENS_JSON,
            TOKENIZER_CONFIG_JSON,
        ]
    }

    /// Default MiniLM model directory relative to the cass data dir.
    pub fn default_model_dir(data_dir: &Path) -> PathBuf {
        data_dir.join("models").join(MINILM_DIR_NAME)
    }

    /// Get model directory for a specific embedder name.
    pub fn model_dir_for(data_dir: &Path, embedder_name: &str) -> Option<PathBuf> {
        let dir_name = match embedder_name {
            "minilm" => MINILM_DIR_NAME,
            "embeddinggemma" => "embeddinggemma-300m",
            "qwen3-embed" => "Qwen3-Embedding-0.6B",
            "modernbert-embed" => "ModernBERT-embed-large",
            "snowflake-arctic-s" => "snowflake-arctic-embed-s",
            "nomic-embed" => "nomic-embed-text-v1.5",
            _ => return None,
        };
        Some(data_dir.join("models").join(dir_name))
    }

    /// Get config for a specific embedder by name.
    pub fn config_for(embedder_name: &str) -> Option<OnnxEmbedderConfig> {
        match embedder_name {
            "minilm" => Some(OnnxEmbedderConfig {
                embedder_id: "minilm-384".to_string(),
                model_id: "all-minilm-l6-v2".to_string(),
                dimension: 384,
                pooling: Pooling::Mean,
            }),
            "embeddinggemma" => Some(OnnxEmbedderConfig {
                embedder_id: "embeddinggemma-256".to_string(),
                model_id: "embeddinggemma-300m".to_string(),
                dimension: 256,
                pooling: Pooling::Mean,
            }),
            "qwen3-embed" => Some(OnnxEmbedderConfig {
                embedder_id: "qwen3-embed-1024".to_string(),
                model_id: "Qwen3-Embedding-0.6B".to_string(),
                dimension: 1024,
                pooling: Pooling::Mean,
            }),
            "modernbert-embed" => Some(OnnxEmbedderConfig {
                embedder_id: "modernbert-embed-768".to_string(),
                model_id: "ModernBERT-embed-large".to_string(),
                dimension: 768,
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
    pub fn load_from_dir(model_dir: &Path) -> EmbedderResult<Self> {
        Self::load_with_config(model_dir, OnnxEmbedderConfig::default())
    }

    /// Load an ONNX embedder with custom configuration.
    pub fn load_with_config(model_dir: &Path, config: OnnxEmbedderConfig) -> EmbedderResult<Self> {
        if !model_dir.is_dir() {
            return Err(EmbedderError::Unavailable(format!(
                "model directory not found: {}",
                model_dir.display()
            )));
        }

        let required = Self::required_model_files();
        let mut missing = Vec::new();
        for name in required {
            let path = model_dir.join(name);
            if !path.is_file() {
                missing.push(*name);
            }
        }
        if !missing.is_empty() {
            return Err(EmbedderError::Unavailable(format!(
                "model files missing in {}: {}",
                model_dir.display(),
                missing.join(", ")
            )));
        }

        let model_file = Self::read_required(model_dir.join(MODEL_FILE), MODEL_FILE)?;
        let tokenizer_file = Self::read_required(model_dir.join(TOKENIZER_JSON), TOKENIZER_JSON)?;
        let config_file = Self::read_required(model_dir.join(CONFIG_JSON), CONFIG_JSON)?;
        let special_tokens_map_file =
            Self::read_required(model_dir.join(SPECIAL_TOKENS_JSON), SPECIAL_TOKENS_JSON)?;
        let tokenizer_config_file =
            Self::read_required(model_dir.join(TOKENIZER_CONFIG_JSON), TOKENIZER_CONFIG_JSON)?;

        let tokenizer_files = TokenizerFiles {
            tokenizer_file,
            config_file,
            special_tokens_map_file,
            tokenizer_config_file,
        };

        let mut model = UserDefinedEmbeddingModel::new(model_file, tokenizer_files);
        model.pooling = Some(config.pooling);

        let init_options = InitOptionsUserDefined::new();

        let model = TextEmbedding::try_new_from_user_defined(model, init_options)
            .map_err(|e| EmbedderError::EmbeddingFailed(format!("fastembed init failed: {e}")))?;

        Ok(Self {
            model: Mutex::new(model),
            id: config.embedder_id,
            model_id: config.model_id,
            dimension: config.dimension,
        })
    }

    /// Load an embedder by name from the data directory.
    pub fn load_by_name(data_dir: &Path, embedder_name: &str) -> EmbedderResult<Self> {
        let model_dir = Self::model_dir_for(data_dir, embedder_name).ok_or_else(|| {
            EmbedderError::Unavailable(format!("unknown embedder: {}", embedder_name))
        })?;
        let config = Self::config_for(embedder_name).ok_or_else(|| {
            EmbedderError::Unavailable(format!("no config for embedder: {}", embedder_name))
        })?;
        Self::load_with_config(&model_dir, config)
    }

    /// Stable model identifier for compatibility checks.
    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    fn read_required(path: PathBuf, label: &str) -> EmbedderResult<Vec<u8>> {
        fs::read(&path).map_err(|e| {
            EmbedderError::Unavailable(format!("unable to read {label} at {}: {e}", path.display()))
        })
    }

    fn normalize_in_place(embedding: &mut [f32]) {
        let norm: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > f32::EPSILON {
            for v in embedding.iter_mut() {
                *v /= norm;
            }
        }
    }
}

impl Embedder for FastEmbedder {
    fn embed(&self, text: &str) -> EmbedderResult<Vec<f32>> {
        if text.is_empty() {
            return Err(EmbedderError::InvalidInput("empty text".to_string()));
        }

        let mut model = self
            .model
            .lock()
            .map_err(|_| EmbedderError::Internal("fastembed lock poisoned".to_string()))?;

        let embeddings = model
            .embed(vec![text], None)
            .map_err(|e| EmbedderError::EmbeddingFailed(format!("fastembed embed failed: {e}")))?;

        let mut embedding = embeddings.into_iter().next().ok_or_else(|| {
            EmbedderError::EmbeddingFailed("fastembed returned no embedding".to_string())
        })?;

        if embedding.len() != self.dimension {
            return Err(EmbedderError::EmbeddingFailed(format!(
                "fastembed dimension mismatch: expected {}, got {}",
                self.dimension,
                embedding.len()
            )));
        }

        Self::normalize_in_place(&mut embedding);
        Ok(embedding)
    }

    fn embed_batch(&self, texts: &[&str]) -> EmbedderResult<Vec<Vec<f32>>> {
        for text in texts {
            if text.is_empty() {
                return Err(EmbedderError::InvalidInput(
                    "empty text in batch".to_string(),
                ));
            }
        }

        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let mut model = self
            .model
            .lock()
            .map_err(|_| EmbedderError::Internal("fastembed lock poisoned".to_string()))?;

        let inputs = texts.to_vec();
        let mut embeddings = model
            .embed(inputs, None)
            .map_err(|e| EmbedderError::EmbeddingFailed(format!("fastembed embed failed: {e}")))?;

        for embedding in embeddings.iter_mut() {
            if embedding.len() != self.dimension {
                return Err(EmbedderError::EmbeddingFailed(format!(
                    "fastembed dimension mismatch: expected {}, got {}",
                    self.dimension,
                    embedding.len()
                )));
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

    fn is_semantic(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fastembed_missing_files_returns_unavailable() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let err = match FastEmbedder::load_from_dir(tmp.path()) {
            Ok(_) => panic!("expected missing-model error"),
            Err(err) => err,
        };
        match err {
            EmbedderError::Unavailable(msg) => {
                assert!(msg.contains("model files missing"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
