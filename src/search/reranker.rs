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
/// Every input must have exactly one finite score under its original ID.
/// Missing, duplicated, foreign or malformed identities are errors, never
/// synthesized zero scores or silent last-write-wins replacements.
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
    let invalid_output = || RerankerError::RerankFailed {
        model: reranker.id().to_string(),
        source: Box::new(std::io::Error::other(
            "reranker must return exactly one finite score for each original document ID",
        )),
    };
    if scores.len() != documents.len() {
        return Err(invalid_output());
    }

    // Backends return relevance order; restore input order only after checking
    // the complete identity mapping. A real score of zero remains valid.
    let mut result = vec![0.0f32; documents.len()];
    let mut seen = vec![false; documents.len()];
    for score in scores {
        let index = score
            .doc_id
            .parse::<usize>()
            .ok()
            .filter(|index| *index < result.len() && score.doc_id == index.to_string())
            .ok_or_else(&invalid_output)?;
        if seen[index] || !score.score.is_finite() {
            return Err(invalid_output());
        }
        seen[index] = true;
        result[index] = score.score;
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
        assert!(display.contains(info.id.as_str()));
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

    // Deliberately synthetic backend: these tests exercise the real bridge's
    // validation and reordering, not native-model accuracy or performance.
    struct ResponseReranker {
        scores: Vec<RerankScore>,
        calls: std::sync::atomic::AtomicUsize,
        fail: bool,
    }

    impl Reranker for ResponseReranker {
        fn rerank_sync(&self, _query: &str, documents: &[RerankDocument]) -> RerankerResult<Vec<RerankScore>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            for (index, document) in documents.iter().enumerate() {
                assert_eq!(document.doc_id, index.to_string());
            }
            if self.fail {
                return Err(RerankerError::RerankerUnavailable { model: self.id().to_string() });
            }
            Ok(self.scores.clone())
        }
        fn id(&self) -> &str { "synthetic-score-contract" }
        fn model_name(&self) -> &str { self.id() }
        fn is_available(&self) -> bool { !self.fail }
    }

    fn response_reranker(entries: &[(&str, f32)]) -> ResponseReranker {
        ResponseReranker {
            scores: entries.iter().enumerate().map(|(rank, (id, score))| RerankScore {
                doc_id: (*id).to_string(), score: *score, original_rank: rank, raw_logit: None,
            }).collect(),
            calls: std::sync::atomic::AtomicUsize::new(0),
            fail: false,
        }
    }

    #[test]
    fn bridge_restores_score_order_without_replacing_real_zero_or_negative_values() {
        let backend = response_reranker(&[("2", -0.5), ("0", 0.75), ("1", 0.0)]);
        assert_eq!(rerank_texts(&backend, "query", &["a", "b", "c"]).unwrap(), [0.75, 0.0, -0.5]);
        assert_eq!(backend.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn bridge_rejects_missing_extra_duplicate_foreign_and_noncanonical_score_ids() {
        for entries in [
            vec![("0", 1.0)],
            vec![("0", 1.0), ("1", 2.0), ("2", 3.0)],
            vec![("0", 1.0), ("0", 2.0)],
            vec![("0", 1.0), ("2", 2.0)],
            vec![("0", 1.0), ("not-an-id", 2.0)],
            vec![("0", 1.0), ("01", 2.0)],
            vec![("0", 1.0), ("+1", 2.0)],
            vec![("0", 1.0), ("-1", 2.0)],
        ] {
            let backend = response_reranker(&entries);
            assert!(matches!(rerank_texts(&backend, "query", &["private-a", "private-b"]), Err(RerankerError::RerankFailed { .. })), "accepted invalid mapping: {entries:?}");
            assert_eq!(backend.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn bridge_rejects_nonfinite_scores_instead_of_serializing_them() {
        for score in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let backend = response_reranker(&[("0", score)]);
            let error = rerank_texts(&backend, "query", &["private-document"]).unwrap_err();
            assert!(matches!(error, RerankerError::RerankFailed { .. }));
            assert!(!error.to_string().contains("private-document"));
        }
    }

    #[test]
    fn bridge_propagates_backend_failure_without_retry_or_fabricated_scores() {
        let mut backend = response_reranker(&[]);
        backend.fail = true;
        assert!(matches!(rerank_texts(&backend, "query", &["document"]), Err(RerankerError::RerankerUnavailable { .. })));
        assert_eq!(backend.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
