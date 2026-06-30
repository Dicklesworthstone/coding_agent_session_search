//! Reranker trait and types for cross-encoder reranking.
//!
//! This module re-exports the canonical [`Reranker`] trait from frankensearch's
//! [`SyncRerank`](frankensearch::SyncRerank) trait. All reranking implementations
//! must satisfy `Reranker`, which provides a synchronous reranking interface
//! suitable for cass's sync call sites.
//!
//! The [`SyncRerankerAdapter`](frankensearch::SyncRerankerAdapter) can wrap any
//! `Reranker` implementor into frankensearch's async `Reranker` trait when needed
//! for the frankensearch search pipeline.
//!
//! # Implementations
//!
//! - **FastEmbed Reranker**: Uses ms-marco-MiniLM-L-6-v2 cross-encoder via FastEmbed.
//!   Requires model download with user consent.

use std::fmt;

pub use frankensearch::SearchError as RerankerError;
pub use frankensearch::SearchResult as RerankerResult;
pub use frankensearch::SyncRerank as Reranker;
pub use frankensearch::{RerankDocument, RerankScore};

/// Convenience function to rerank raw text documents.
///
/// Wraps `&[&str]` documents into [`RerankDocument`] structs and extracts
/// the resulting scores back into a `Vec<f32>` in original document order.
pub fn rerank_texts(
    reranker: &dyn Reranker,
    query: &str,
    documents: &[&str],
) -> RerankerResult<Vec<f32>> {
    let rerank_docs: Vec<RerankDocument> = documents
        .iter()
        .enumerate()
        .map(|(i, text)| RerankDocument {
            doc_id: i.to_string(),
            text: text.to_string(),
        })
        .collect();

    let scores = reranker.rerank_sync(query, &rerank_docs)?;

    // Convert RerankScore vec back to Vec<f32> in original document order
    let mut result = vec![0.0f32; documents.len()];
    for rs in &scores {
        if let Ok(idx) = rs.doc_id.parse::<usize>()
            && idx < result.len()
        {
            result[idx] = rs.score;
        }
    }
    Ok(result)
}

/// Metadata about a reranker for display and logging.
#[derive(Debug, Clone)]
pub struct RerankerInfo {
    /// The reranker's unique identifier.
    pub id: String,
    /// Whether the reranker is available.
    pub is_available: bool,
}

impl RerankerInfo {
    /// Create info from a reranker instance.
    pub fn from_reranker(reranker: &dyn Reranker) -> Self {
        Self {
            id: reranker.id().to_string(),
            is_available: reranker.is_available(),
        }
    }
}

impl fmt::Display for RerankerInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let status = if self.is_available {
            "available"
        } else {
            "unavailable"
        };
        write!(f, "{} ({})", self.id, status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::fastembed_reranker::FastEmbedReranker;
    use std::path::PathBuf;

    fn fastembed_fixture_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/models/xenova-ms-marco-minilm-l6-v2-int8")
    }

    // cass #308: the pure-Rust native reranker loads f32 `model.safetensors` of the
    // ms-marco-MiniLM-L6 topology; it cannot load the small committed int8 ONNX
    // fixture. Tests using this helper are `#[ignore]`d by default and run against a
    // real model supplied via `FRANKENSEARCH_MODEL_DIR` (`cargo test -- --ignored`).
    fn load_fastembed_fixture() -> FastEmbedReranker {
        let dir = dotenvy::var("FRANKENSEARCH_MODEL_DIR")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(fastembed_fixture_dir);
        FastEmbedReranker::load_from_dir(&dir).expect("fastembed reranker fixture should load")
    }

    #[test]
    #[ignore = "needs a real safetensors ms-marco-MiniLM model via FRANKENSEARCH_MODEL_DIR; the int8 ONNX fixture is incompatible with the native backend — cass #308"]
    fn test_reranker_trait_basic() {
        let reranker = load_fastembed_fixture();
        let scores = rerank_texts(
            &reranker,
            "test query",
            &["short", "medium length doc", "longer document text"],
        )
        .unwrap();

        assert_eq!(scores.len(), 3);
        for score in scores {
            assert!(score.is_finite());
        }
    }

    #[test]
    fn test_reranker_unavailable() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let err = match FastEmbedReranker::load_from_dir(tmp.path()) {
            Ok(_) => panic!("expected unavailable error"),
            Err(err) => err,
        };
        assert!(matches!(
            err,
            RerankerError::RerankFailed { .. }
                | RerankerError::EmbedderUnavailable { .. }
                | RerankerError::RerankerUnavailable { .. }
        ));
    }

    #[test]
    #[ignore = "needs a real safetensors ms-marco-MiniLM model via FRANKENSEARCH_MODEL_DIR; the int8 ONNX fixture is incompatible with the native backend — cass #308"]
    fn test_reranker_empty_query_error() {
        let reranker = load_fastembed_fixture();
        let result = rerank_texts(&reranker, "", &["doc"]);
        assert!(result.is_err());
    }

    #[test]
    #[ignore = "needs a real safetensors ms-marco-MiniLM model via FRANKENSEARCH_MODEL_DIR; the int8 ONNX fixture is incompatible with the native backend — cass #308"]
    fn test_reranker_empty_documents_error() {
        let reranker = load_fastembed_fixture();
        let result = rerank_texts(&reranker, "query", &[]);
        assert!(result.is_err());
    }

    #[test]
    #[ignore = "needs a real safetensors ms-marco-MiniLM model via FRANKENSEARCH_MODEL_DIR; the int8 ONNX fixture is incompatible with the native backend — cass #308"]
    fn test_reranker_info() {
        let reranker = load_fastembed_fixture();
        let info = RerankerInfo::from_reranker(&reranker);

        assert_eq!(info.id, FastEmbedReranker::reranker_id_static());
        assert!(info.is_available);

        let display = format!("{info}");
        assert!(display.contains(FastEmbedReranker::reranker_id_static()));
        assert!(display.contains("available"));
    }

    #[test]
    fn test_reranker_error_display() {
        let err = RerankerError::RerankFailed {
            model: "test".to_string(),
            source: Box::new(std::io::Error::other("inference error")),
        };
        assert!(err.to_string().contains("inference error"));
    }
}
