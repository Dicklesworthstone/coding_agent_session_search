//! FastEmbed-based cross-encoder reranker (ms-marco-MiniLM-L-6-v2).
//!
//! Re-exports [`FastEmbedReranker`] from `frankensearch::rerank::fastembed_reranker`.
//! The implementation lives in the `frankensearch-rerank` crate.
//!
//! # `semantic` feature gate (cass#256)
//!
//! When the `semantic` Cargo feature is **disabled** (i.e. baseline build), the
//! upstream `frankensearch::FastEmbedReranker` is not available because
//! `frankensearch/fastembed-reranker` is the feature path that drags in
//! `fastembed` and the prebuilt Microsoft ONNX Runtime binary. In that build a
//! local stub `FastEmbedReranker` is exposed: it has the same public surface
//! the rest of the crate relies on (`default_model_dir`, `load_from_dir`,
//! `reranker_id_static`) but the loader returns a stable
//! `RerankerError::RerankerUnavailable` and the reranker cannot be
//! instantiated. Lexical search remains fully available.

#[cfg(feature = "semantic")]
pub use frankensearch::FastEmbedReranker;

#[cfg(not(feature = "semantic"))]
pub use stub::FastEmbedReranker;

#[cfg(not(feature = "semantic"))]
mod stub {
    use std::path::{Path, PathBuf};

    use crate::search::reranker::{Reranker, RerankerError, RerankerResult};
    use frankensearch::{RerankDocument, RerankScore};

    const MS_MARCO_RERANKER_ID: &str = "ms-marco-minilm-l6-v2";
    const MS_MARCO_DIR_NAME: &str = "ms-marco-MiniLM-L-6-v2";

    /// Baseline-build stub for the cross-encoder reranker.
    ///
    /// `FastEmbedReranker` cannot actually be instantiated in this build -
    /// [`load_from_dir`] always returns `RerankerError::RerankerUnavailable`.
    /// The struct and `Reranker` impl exist purely so existing
    /// `Arc<dyn Reranker>` plumbing (`reranker_registry`, `daemon::models`, etc.)
    /// keeps compiling.
    pub struct FastEmbedReranker {
        _private: (),
    }

    impl FastEmbedReranker {
        /// Stable reranker identifier (matches the upstream constant so
        /// metadata/JSON contracts remain stable across baseline and full
        /// builds).
        pub fn reranker_id_static() -> &'static str {
            MS_MARCO_RERANKER_ID
        }

        /// Default model directory relative to the cass data dir. Mirrors
        /// the layout used by the full build so the model_manager's
        /// "is this on disk?" probes return the same answer either way.
        pub fn default_model_dir(data_dir: &Path) -> PathBuf {
            data_dir.join("models").join(MS_MARCO_DIR_NAME)
        }

        /// Baseline-build stub: see the module-level note on cass#256.
        pub fn load_from_dir(_model_dir: &Path) -> RerankerResult<Self> {
            Err(RerankerError::RerankerUnavailable {
                model: MS_MARCO_RERANKER_ID.to_string(),
            })
        }
    }

    impl Reranker for FastEmbedReranker {
        fn rerank_sync(
            &self,
            _query: &str,
            _documents: &[RerankDocument],
        ) -> RerankerResult<Vec<RerankScore>> {
            Err(RerankerError::RerankerUnavailable {
                model: MS_MARCO_RERANKER_ID.to_string(),
            })
        }

        fn id(&self) -> &str {
            MS_MARCO_RERANKER_ID
        }

        fn model_name(&self) -> &str {
            MS_MARCO_DIR_NAME
        }

        fn is_available(&self) -> bool {
            false
        }
    }
}
