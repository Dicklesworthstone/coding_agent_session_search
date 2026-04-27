//! Embedder trait and types for semantic search.
//!
//! This module re-exports the canonical [`Embedder`] trait from frankensearch's
//! [`SyncEmbed`](frankensearch::SyncEmbed) trait. All embedding implementations
//! must satisfy `Embedder`, which provides a synchronous embedding interface
//! suitable for cass's sync call sites.
//!
//! The [`SyncEmbedderAdapter`](frankensearch::SyncEmbedderAdapter) can wrap any
//! `Embedder` implementor into frankensearch's async `Embedder` trait when needed
//! for the frankensearch search pipeline.
//!
//! # Implementations
//!
//! - **Hash embedder**: FNV-1a feature hashing (always available, ~256 dimensions)
//! - **ML embedder**: FastEmbed with the MiniLM model (requires model download, 384 dimensions)

use std::fmt;

pub use frankensearch::SearchError as EmbedderError;
pub use frankensearch::SearchResult as EmbedderResult;
pub use frankensearch::SyncEmbed as Embedder;

/// Metadata about an embedder for display and logging.
#[derive(Debug, Clone)]
pub struct EmbedderInfo {
    /// The embedder's unique identifier.
    pub id: String,
    /// The output dimension.
    pub dimension: usize,
    /// Whether it's a semantic (ML) embedder.
    pub is_semantic: bool,
}

impl EmbedderInfo {
    /// Create info from an embedder instance.
    pub fn from_embedder(embedder: &dyn Embedder) -> Self {
        Self {
            id: embedder.id().to_string(),
            dimension: embedder.dimension(),
            is_semantic: embedder.is_semantic(),
        }
    }
}

impl fmt::Display for EmbedderInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = if self.is_semantic {
            "semantic"
        } else {
            "lexical"
        };
        write!(f, "{} ({}, {} dims)", self.id, kind, self.dimension)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::fastembed_embedder::FastEmbedder;
    use crate::search::hash_embedder::HashEmbedder;
    use std::path::PathBuf;

    fn fastembed_fixture_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/models/xenova-paraphrase-minilm-l3-v2-int8")
    }

    fn load_fastembed_fixture() -> FastEmbedder {
        FastEmbedder::load_from_dir(&fastembed_fixture_dir())
            .expect("fastembed fixture should load")
    }

    #[test]
    fn test_embedder_trait_basic() {
        let embedder = HashEmbedder::new(256);
        let embedding = embedder.embed_sync("hello world").unwrap();
        assert_eq!(embedding.len(), 256);
        assert_eq!(embedder.id(), "fnv1a-256");
        assert!(!embedder.is_semantic());
    }

    #[test]
    fn test_embedder_trait_semantic() {
        let embedder = load_fastembed_fixture();
        assert_eq!(embedder.dimension(), 384);
        assert_eq!(embedder.id(), FastEmbedder::embedder_id_static());
        assert!(embedder.is_semantic());
    }

    #[test]
    fn test_embedder_batch() {
        let embedder = load_fastembed_fixture();
        let texts = &["hello", "world", "test"];
        let embeddings = embedder.embed_batch_sync(texts).unwrap();

        assert_eq!(embeddings.len(), 3);
        for embedding in &embeddings {
            assert_eq!(embedding.len(), 384);
        }
    }

    #[test]
    fn test_embedder_empty_input_error() {
        let embedder = load_fastembed_fixture();
        let result = embedder.embed_sync("");
        assert!(result.is_err());
    }

    #[test]
    fn test_embedder_info() {
        let embedder = load_fastembed_fixture();
        let info = EmbedderInfo::from_embedder(&embedder);
        assert_eq!(info.id, FastEmbedder::embedder_id_static());
        assert_eq!(info.dimension, 384);
        assert!(info.is_semantic);

        let display = format!("{info}");
        for expected in [FastEmbedder::embedder_id_static(), "semantic", "384"] {
            assert!(
                display.contains(expected),
                "display {display:?} should contain {expected:?}"
            );
        }
    }

    #[test]
    fn test_embedder_error_display() {
        let err = EmbedderError::EmbedderUnavailable {
            model: "test".to_string(),
            reason: "model not downloaded".to_string(),
        };
        assert!(err.to_string().contains("model not downloaded"));

        let err = EmbedderError::EmbeddingFailed {
            model: "test".to_string(),
            source: Box::new(std::io::Error::other("inference error")),
        };
        assert!(err.to_string().contains("inference error"));
    }
}
