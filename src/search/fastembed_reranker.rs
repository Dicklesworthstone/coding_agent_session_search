//! Cross-encoder reranker (ms-marco-MiniLM-L-6-v2), pure-Rust via frankentorch.
//!
//! Thin cass-side wrapper around [`frankensearch::NativeReranker`] — a pure-Rust
//! (frankentorch) cross-encoder with **no ONNX Runtime / no `ort`** — that adds
//! the cass-specific static helpers (`reranker_id_static`, `default_model_dir`,
//! `load_from_dir`) the reranker registry + model management rely on. The type is
//! still named `FastEmbedReranker` for call-site stability; the FastEmbed / ONNX
//! backend was removed in cass #308 (see `fastembed_embedder` for the rationale).

use std::path::{Path, PathBuf};

use crate::search::reranker::{Reranker, RerankerError, RerankerResult};
use frankensearch::{NativeReranker, RerankDocument, RerankScore};

const MS_MARCO_RERANKER_ID: &str = "ms-marco-minilm-l6-v2";
const MS_MARCO_DIR_NAME: &str = "ms-marco-MiniLM-L-6-v2";

/// Pure-Rust cross-encoder reranker, wrapping [`frankensearch::NativeReranker`].
pub struct FastEmbedReranker {
    inner: NativeReranker,
}

impl FastEmbedReranker {
    /// Stable reranker identifier (matches the existing cass metadata/JSON
    /// contracts so index naming + goldens remain stable).
    pub fn reranker_id_static() -> &'static str {
        MS_MARCO_RERANKER_ID
    }

    /// Default model directory relative to the cass data dir.
    pub fn default_model_dir(data_dir: &Path) -> PathBuf {
        data_dir.join("models").join(MS_MARCO_DIR_NAME)
    }

    /// Load the cross-encoder from a model directory containing a safetensors
    /// weight file + `tokenizer.json`.
    pub fn load_from_dir(model_dir: &Path) -> RerankerResult<Self> {
        let inner = NativeReranker::load(model_dir).map_err(|e| match e {
            RerankerError::RerankerUnavailable { model } => {
                RerankerError::RerankerUnavailable { model }
            }
            other => RerankerError::RerankFailed {
                model: MS_MARCO_RERANKER_ID.to_string(),
                source: format!("native reranker load failed: {other}").into(),
            },
        })?;
        Ok(Self { inner })
    }
}

impl Reranker for FastEmbedReranker {
    fn rerank_sync(
        &self,
        query: &str,
        documents: &[RerankDocument],
    ) -> RerankerResult<Vec<RerankScore>> {
        self.inner.rerank_sync(query, documents)
    }

    fn id(&self) -> &str {
        MS_MARCO_RERANKER_ID
    }

    fn model_name(&self) -> &str {
        MS_MARCO_DIR_NAME
    }

    fn is_available(&self) -> bool {
        true
    }
}
