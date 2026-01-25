//! Reranker trait and types for cross-encoder reranking.
//!
//! This module defines the [`Reranker`] trait that all reranking implementations must satisfy.
//! A reranker takes a query and a list of documents and produces relevance scores for each
//! query-document pair using a cross-encoder model that attends to both simultaneously.
//!
//! # Implementations
//!
//! - **FastEmbed Reranker**: Uses ms-marco-MiniLM-L-6-v2 cross-encoder via FastEmbed.
//!   Requires model download with user consent.
//!
//! # Example
//!
//! ```ignore
//! use crate::search::reranker::{Reranker, RerankerError};
//!
//! fn rerank_results(reranker: &dyn Reranker, query: &str, docs: &[&str]) -> Result<Vec<(usize, f32)>, RerankerError> {
//!     let scores = reranker.rerank(query, docs)?;
//!     // scores[i] is the relevance score for docs[i]
//!     let mut indexed: Vec<_> = scores.into_iter().enumerate().collect();
//!     indexed.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
//!     Ok(indexed)
//! }
//! ```

use std::fmt;

/// Error type for reranker operations.
#[derive(Debug)]
pub enum RerankerError {
    /// The reranker is not available (e.g., model not downloaded).
    Unavailable(String),
    /// Failed to rerank the input.
    RerankFailed(String),
    /// Input is empty or invalid.
    InvalidInput(String),
    /// Internal error in the reranker.
    Internal(String),
}

impl fmt::Display for RerankerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RerankerError::Unavailable(msg) => write!(f, "reranker unavailable: {msg}"),
            RerankerError::RerankFailed(msg) => write!(f, "rerank failed: {msg}"),
            RerankerError::InvalidInput(msg) => write!(f, "invalid input: {msg}"),
            RerankerError::Internal(msg) => write!(f, "internal error: {msg}"),
        }
    }
}

impl std::error::Error for RerankerError {}

/// Result type for reranker operations.
pub type RerankerResult<T> = Result<T, RerankerError>;

/// Trait for cross-encoder reranking implementations.
///
/// Rerankers score query-document pairs using a cross-encoder that attends to both
/// query and document simultaneously. This provides more accurate relevance scores
/// than bi-encoder (embedding-based) similarity but at higher computational cost.
///
/// # Thread Safety
///
/// Implementations should be `Send + Sync` to allow use across threads.
pub trait Reranker: Send + Sync {
    /// Rerank documents against a query.
    ///
    /// # Arguments
    ///
    /// * `query` - The search query.
    /// * `documents` - Slice of documents to rerank.
    ///
    /// # Returns
    ///
    /// A vector of relevance scores, one per document, in the same order as input.
    /// Higher scores indicate more relevant documents.
    ///
    /// # Errors
    ///
    /// - [`RerankerError::InvalidInput`] if query or any document is empty.
    /// - [`RerankerError::Unavailable`] if the reranker is not ready.
    /// - [`RerankerError::RerankFailed`] if reranking fails.
    fn rerank(&self, query: &str, documents: &[&str]) -> RerankerResult<Vec<f32>>;

    /// Unique identifier for this reranker.
    ///
    /// Format: `{model}-{version}`
    ///
    /// # Examples
    ///
    /// - `"ms-marco-minilm-l6-v2"` for the default MiniLM reranker
    fn id(&self) -> &str;

    /// Whether this reranker is available and ready to use.
    fn is_available(&self) -> bool;
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

    /// Mock reranker for testing.
    struct MockReranker {
        available: bool,
    }

    impl Reranker for MockReranker {
        fn rerank(&self, query: &str, documents: &[&str]) -> RerankerResult<Vec<f32>> {
            if query.is_empty() {
                return Err(RerankerError::InvalidInput("empty query".to_string()));
            }
            if documents.is_empty() {
                return Err(RerankerError::InvalidInput("empty documents".to_string()));
            }
            if !self.available {
                return Err(RerankerError::Unavailable("mock unavailable".to_string()));
            }
            // Mock scoring: longer documents score higher
            Ok(documents
                .iter()
                .map(|d| d.len() as f32 / 100.0)
                .collect())
        }

        fn id(&self) -> &str {
            "mock-reranker"
        }

        fn is_available(&self) -> bool {
            self.available
        }
    }

    #[test]
    fn test_reranker_trait_basic() {
        let reranker = MockReranker { available: true };

        let docs = ["short", "medium length doc", "this is a much longer document"];
        let scores = reranker.rerank("test query", &docs).unwrap();

        assert_eq!(scores.len(), 3);
        // Longer docs should score higher in our mock
        assert!(scores[2] > scores[1]);
        assert!(scores[1] > scores[0]);
    }

    #[test]
    fn test_reranker_unavailable() {
        let reranker = MockReranker { available: false };

        let result = reranker.rerank("query", &["doc"]);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), RerankerError::Unavailable(_)));
    }

    #[test]
    fn test_reranker_empty_query_error() {
        let reranker = MockReranker { available: true };

        let result = reranker.rerank("", &["doc"]);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), RerankerError::InvalidInput(_)));
    }

    #[test]
    fn test_reranker_info() {
        let reranker = MockReranker { available: true };
        let info = RerankerInfo::from_reranker(&reranker);

        assert_eq!(info.id, "mock-reranker");
        assert!(info.is_available);

        let display = format!("{info}");
        assert!(display.contains("mock-reranker"));
        assert!(display.contains("available"));
    }

    #[test]
    fn test_reranker_error_display() {
        let err = RerankerError::Unavailable("model not downloaded".to_string());
        assert!(err.to_string().contains("unavailable"));

        let err = RerankerError::RerankFailed("inference error".to_string());
        assert!(err.to_string().contains("rerank failed"));

        let err = RerankerError::InvalidInput("empty".to_string());
        assert!(err.to_string().contains("invalid input"));

        let err = RerankerError::Internal("panic".to_string());
        assert!(err.to_string().contains("internal error"));
    }
}
