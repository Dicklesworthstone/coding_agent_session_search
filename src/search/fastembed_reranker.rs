//! FastEmbed-based cross-encoder reranker (ms-marco-MiniLM-L-6-v2).
//!
//! Loads a local ONNX model + tokenizer bundle and produces relevance scores for
//! query-document pairs. This implementation never downloads model assets; it expects
//! the model files to be present on disk and returns a clear error when they are missing.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use fastembed::{RerankInitOptionsUserDefined, TextRerank, UserDefinedRerankingModel};

use super::reranker::{Reranker, RerankerError, RerankerResult};

const MODEL_ID: &str = "ms-marco-minilm-l6-v2";
const MODEL_DIR_NAME: &str = "ms-marco-MiniLM-L-6-v2";
const RERANKER_ID: &str = "ms-marco-minilm-l6-v2";

const MODEL_FILE: &str = "model.onnx";
const TOKENIZER_JSON: &str = "tokenizer.json";
const CONFIG_JSON: &str = "config.json";
const SPECIAL_TOKENS_JSON: &str = "special_tokens_map.json";
const TOKENIZER_CONFIG_JSON: &str = "tokenizer_config.json";

/// FastEmbed-backed cross-encoder reranker using ms-marco-MiniLM-L-6-v2.
///
/// This reranker uses a cross-encoder model that processes query-document pairs
/// simultaneously, providing more accurate relevance scores than bi-encoder similarity.
/// The MiniLM-L-6-v2 model is optimized for fast inference on CPU.
pub struct FastEmbedReranker {
    model: Mutex<TextRerank>,
    id: String,
    model_id: String,
}

impl FastEmbedReranker {
    /// Stable reranker identifier for ms-marco-MiniLM-L-6-v2.
    pub fn reranker_id_static() -> &'static str {
        RERANKER_ID
    }

    /// Stable model identifier for ms-marco-MiniLM-L-6-v2.
    pub fn model_id_static() -> &'static str {
        MODEL_ID
    }

    /// Required model files for the reranker (must all exist locally).
    pub fn required_model_files() -> &'static [&'static str] {
        &[
            MODEL_FILE,
            TOKENIZER_JSON,
            CONFIG_JSON,
            SPECIAL_TOKENS_JSON,
            TOKENIZER_CONFIG_JSON,
        ]
    }

    /// Default model directory relative to the cass data dir.
    pub fn default_model_dir(data_dir: &Path) -> PathBuf {
        data_dir.join("models").join(MODEL_DIR_NAME)
    }

    /// Load the reranker model + tokenizer from a local directory.
    ///
    /// This never downloads; it returns `RerankerError::Unavailable` if any
    /// required file is missing.
    pub fn load_from_dir(model_dir: &Path) -> RerankerResult<Self> {
        if !model_dir.is_dir() {
            return Err(RerankerError::Unavailable(format!(
                "reranker model directory not found: {}",
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
            return Err(RerankerError::Unavailable(format!(
                "reranker model files missing in {}: {}",
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

        let tokenizer_files = fastembed::TokenizerFiles {
            tokenizer_file,
            config_file,
            special_tokens_map_file,
            tokenizer_config_file,
        };

        let model = UserDefinedRerankingModel::new(model_file, tokenizer_files);
        let init_options = RerankInitOptionsUserDefined::new();

        let model = TextRerank::try_new_from_user_defined(model, init_options)
            .map_err(|e| RerankerError::RerankFailed(format!("fastembed reranker init failed: {e}")))?;

        Ok(Self {
            model: Mutex::new(model),
            id: RERANKER_ID.to_string(),
            model_id: MODEL_ID.to_string(),
        })
    }

    /// Stable model identifier for compatibility checks.
    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    fn read_required(path: PathBuf, label: &str) -> RerankerResult<Vec<u8>> {
        fs::read(&path).map_err(|e| {
            RerankerError::Unavailable(format!(
                "unable to read {label} at {}: {e}",
                path.display()
            ))
        })
    }
}

impl Reranker for FastEmbedReranker {
    fn rerank(&self, query: &str, documents: &[&str]) -> RerankerResult<Vec<f32>> {
        if query.is_empty() {
            return Err(RerankerError::InvalidInput("empty query".to_string()));
        }
        if documents.is_empty() {
            return Err(RerankerError::InvalidInput("empty documents list".to_string()));
        }
        for (i, doc) in documents.iter().enumerate() {
            if doc.is_empty() {
                return Err(RerankerError::InvalidInput(format!(
                    "empty document at index {i}"
                )));
            }
        }

        let model = self
            .model
            .lock()
            .map_err(|_| RerankerError::Internal("fastembed reranker lock poisoned".to_string()))?;

        // Convert to Vec<String> for fastembed API
        let doc_strings: Vec<String> = documents.iter().map(|s| s.to_string()).collect();

        // FastEmbed's rerank returns Vec<RerankResult> with index and score
        let rerank_results = model
            .rerank(query.to_string(), doc_strings, false, None)
            .map_err(|e| RerankerError::RerankFailed(format!("fastembed rerank failed: {e}")))?;

        // Convert to scores in original document order
        let mut scores = vec![0.0f32; documents.len()];
        for result in rerank_results {
            if result.index < scores.len() {
                scores[result.index] = result.score;
            }
        }

        Ok(scores)
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn is_available(&self) -> bool {
        true // If we got this far, the model is loaded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reranker_missing_files_returns_unavailable() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let err = match FastEmbedReranker::load_from_dir(tmp.path()) {
            Ok(_) => panic!("expected missing-model error"),
            Err(err) => err,
        };
        match err {
            RerankerError::Unavailable(msg) => {
                assert!(msg.contains("model files missing"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn reranker_required_files() {
        let files = FastEmbedReranker::required_model_files();
        assert!(files.contains(&"model.onnx"));
        assert!(files.contains(&"tokenizer.json"));
        assert!(files.contains(&"config.json"));
    }
}
