//! FNV-1a feature hashing embedder.
//!
//! This module provides a deterministic, fast embedder that uses FNV-1a hashing
//! to project text into a fixed-dimension vector space. While not "truly" semantic
//! (it captures lexical overlap rather than meaning), it provides:
//!
//! - **Instant embedding**: No model loading, no initialization delay
//! - **Deterministic output**: Same input always produces same output
//! - **Zero network dependency**: Works offline, no downloads required
//!
//! # Algorithm
//!
//! 1. **Tokenize**: Lowercase, split on non-alphanumeric, filter tokens with len < 2
//! 2. **Hash**: Apply FNV-1a to each token
//! 3. **Project**: Use hash to determine dimension index and sign (+1 or -1)
//! 4. **Normalize**: L2 normalize the resulting vector to unit length
//!
//! # When to Use
//!
//! - When ML model is not installed
//! - When user explicitly opts for hash mode (`CASS_SEMANTIC_EMBEDDER=hash`)
//! - As a fallback when ML inference fails
//!
//! # Example
//!
//! ```ignore
//! use crate::search::embedder::Embedder;
//! use crate::search::hash_embedder::HashEmbedder;
//!
//! let embedder = HashEmbedder::new(384);
//! let embedding = embedder.embed_sync("hello world").unwrap();
//! assert_eq!(embedding.len(), 384);
//! ```

use super::embedder::{Embedder, EmbedderError, EmbedderResult};
use frankensearch::{
    HashAlgorithm as FsHashAlgorithm, HashEmbedder as FsHashEmbedder, ModelCategory, ModelTier,
};

/// Default embedding dimension (matches MiniLM for compatibility).
pub const DEFAULT_DIMENSION: usize = 384;

/// Minimum token length to include in embedding.
const MIN_TOKEN_LEN: usize = 2;

/// FNV-1a feature hashing embedder.
///
/// Projects text into a fixed-dimension vector using FNV-1a hashing.
/// Each token contributes to one dimension, with the hash determining
/// both which dimension and the sign (+1/-1) of the contribution.
#[derive(Debug, Clone)]
pub struct HashEmbedder {
    dimension: usize,
    id: String,
    delegate: FsHashEmbedder,
}

impl HashEmbedder {
    /// Create a new hash embedder with the specified dimension.
    ///
    /// # Arguments
    ///
    /// * `dimension` - The output vector dimension. Common values: 256, 384, 512.
    ///   Higher dimensions reduce hash collisions but increase storage.
    ///
    /// # Panics
    ///
    /// Panics if dimension is 0.
    pub fn new(dimension: usize) -> Self {
        assert!(dimension > 0, "dimension must be positive");
        Self {
            dimension,
            id: format!("fnv1a-{dimension}"),
            delegate: FsHashEmbedder::new(dimension, FsHashAlgorithm::FnvModular),
        }
    }

    /// Create a new hash embedder with the default dimension (384).
    pub fn default_dimension() -> Self {
        Self::new(DEFAULT_DIMENSION)
    }

    /// Tokenize text into lowercase alphanumeric tokens.
    ///
    /// Splits on non-alphanumeric characters and filters tokens shorter than
    /// `MIN_TOKEN_LEN`. This provides basic word extraction suitable for
    /// feature hashing.
    fn tokenize(text: &str) -> Vec<String> {
        text.to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|s| s.chars().count() >= MIN_TOKEN_LEN)
            .map(String::from)
            .collect()
    }

    fn uniform_fallback(&self) -> Vec<f32> {
        let mut embedding = vec![1.0f32; self.dimension];
        let norm = (self.dimension as f32).sqrt();
        for value in &mut embedding {
            *value /= norm;
        }
        embedding
    }
}

impl Default for HashEmbedder {
    fn default() -> Self {
        Self::default_dimension()
    }
}

impl Embedder for HashEmbedder {
    fn embed_sync(&self, text: &str) -> EmbedderResult<Vec<f32>> {
        if text.is_empty() {
            return Err(EmbedderError::InvalidConfig {
                field: "input_text".to_string(),
                value: "(empty)".to_string(),
                reason: "empty text".to_string(),
            });
        }

        let tokens = Self::tokenize(text);

        // Preserve cass legacy behavior for low-signal inputs.
        if tokens.is_empty() {
            return Ok(self.uniform_fallback());
        }

        // Delegate core hashing/projection logic to frankensearch implementation.
        // We pass canonicalized text to preserve cass's case-insensitive semantics.
        let canonical = tokens.join(" ");
        let embedding = self.delegate.embed_sync(&canonical);
        if embedding.len() != self.dimension {
            return Err(EmbedderError::EmbeddingFailed {
                model: self.id.clone(),
                source: Box::new(std::io::Error::other(format!(
                    "delegate dimension mismatch: expected {}, got {}",
                    self.dimension,
                    embedding.len()
                ))),
            });
        }
        Ok(embedding)
    }

    fn embed_batch_sync(&self, texts: &[&str]) -> EmbedderResult<Vec<Vec<f32>>> {
        texts.iter().map(|t| self.embed_sync(t)).collect()
    }

    fn dimension(&self) -> usize {
        self.dimension
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn is_semantic(&self) -> bool {
        false
    }

    fn category(&self) -> ModelCategory {
        ModelCategory::HashEmbedder
    }

    fn tier(&self) -> ModelTier {
        ModelTier::Fast
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_embedder_basic() {
        let embedder = HashEmbedder::new(256);
        let embedding = embedder.embed_sync("hello world").unwrap();

        assert_eq!(embedding.len(), 256);
        assert_eq!(embedder.id(), "fnv1a-256");
        assert!(!embedder.is_semantic());
    }

    #[test]
    fn test_hash_embedder_default() {
        let embedder = HashEmbedder::default();

        assert_eq!(embedder.dimension(), DEFAULT_DIMENSION);
        assert_eq!(embedder.id(), format!("fnv1a-{DEFAULT_DIMENSION}"));
    }

    #[test]
    fn test_hash_embedder_deterministic() {
        let embedder = HashEmbedder::new(256);

        let text = "deterministic embedding test with some words";
        let embedding1 = embedder.embed_sync(text).unwrap();
        let embedding2 = embedder.embed_sync(text).unwrap();

        // Exact same output
        assert_eq!(embedding1, embedding2);
    }

    #[test]
    fn test_hash_embedder_l2_normalized() {
        let embedder = HashEmbedder::new(256);
        let embedding = embedder.embed_sync("normalize this vector").unwrap();

        // Compute L2 norm
        let norm: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();

        // Should be approximately 1.0
        assert!(
            (norm - 1.0).abs() < 1e-5,
            "L2 norm should be ~1.0, got {norm}"
        );
    }

    #[test]
    fn test_hash_embedder_different_texts_different_embeddings() {
        let embedder = HashEmbedder::new(256);

        let embedding1 = embedder.embed_sync("hello world").unwrap();
        let embedding2 = embedder.embed_sync("goodbye world").unwrap();

        // Should be different
        assert_ne!(embedding1, embedding2);
    }

    #[test]
    fn test_hash_embedder_empty_input_error() {
        let embedder = HashEmbedder::new(256);
        let result = embedder.embed_sync("");

        assert!(result.is_err());
    }

    #[test]
    fn test_hash_embedder_punctuation_only() {
        let embedder = HashEmbedder::new(256);

        // Should handle gracefully (all tokens filtered out)
        let embedding = embedder.embed_sync("!@#$%^&*()").unwrap();

        assert_eq!(embedding.len(), 256);
        // Still normalized
        let norm: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-5,
            "L2 norm should be ~1.0, got {norm}"
        );
    }

    #[test]
    fn test_hash_embedder_batch() {
        let embedder = HashEmbedder::new(256);
        let texts = &["hello world", "goodbye world", "test batch"];

        let embeddings = embedder.embed_batch_sync(texts).unwrap();

        assert_eq!(embeddings.len(), 3);
        for embedding in &embeddings {
            assert_eq!(embedding.len(), 256);

            // Each should be normalized
            let norm: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!(
                (norm - 1.0).abs() < 1e-5,
                "L2 norm should be ~1.0, got {norm}"
            );
        }
    }

    #[test]
    fn test_hash_embedder_batch_empty_error() {
        let embedder = HashEmbedder::new(256);
        let texts = &["hello", "", "world"];

        let result = embedder.embed_batch_sync(texts);
        assert!(result.is_err());
    }

    #[test]
    fn test_tokenize() {
        let tokens = HashEmbedder::tokenize("Hello, World! This is a TEST-123.");

        // Should be lowercase, split on non-alphanumeric, filter short tokens
        for expected in ["hello", "world", "this", "test", "123", "is"] {
            assert!(
                tokens.iter().any(|candidate| candidate == expected),
                "expected token {expected:?} in {tokens:?}"
            );
        }

        // Single characters should be filtered (len < 2)
        assert!(
            !tokens.iter().any(|candidate| candidate == "a"),
            "single-character token should be filtered: {tokens:?}"
        );
    }

    #[test]
    fn test_tokenize_includes_len_2() {
        let tokens = HashEmbedder::tokenize("is it ok");

        // Tokens with len >= 2 should be included
        assert!(tokens.contains(&"is".to_string()));
        assert!(tokens.contains(&"it".to_string()));
        assert!(tokens.contains(&"ok".to_string()));
    }

    #[test]
    fn test_case_insensitivity() {
        let embedder = HashEmbedder::new(256);

        let embedding1 = embedder.embed_sync("Hello World").unwrap();
        let embedding2 = embedder.embed_sync("hello world").unwrap();
        let embedding3 = embedder.embed_sync("HELLO WORLD").unwrap();

        // All should produce the same embedding (case insensitive)
        assert_eq!(embedding1, embedding2);
        assert_eq!(embedding2, embedding3);
    }

    #[test]
    fn test_whitespace_insensitivity() {
        let embedder = HashEmbedder::new(256);

        let embedding1 = embedder.embed_sync("hello   world").unwrap();
        let embedding2 = embedder.embed_sync("hello world").unwrap();
        let embedding3 = embedder.embed_sync("hello\n\tworld").unwrap();

        // All should produce the same embedding (whitespace collapsed)
        assert_eq!(embedding1, embedding2);
        assert_eq!(embedding2, embedding3);
    }

    #[test]
    #[should_panic(expected = "dimension must be positive")]
    fn test_zero_dimension_panics() {
        let _ = HashEmbedder::new(0);
    }

    #[test]
    fn test_large_dimension() {
        let embedder = HashEmbedder::new(4096);
        let embedding = embedder.embed_sync("test large dimension").unwrap();

        assert_eq!(embedding.len(), 4096);

        // Still normalized
        let norm: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-5,
            "L2 norm should be ~1.0, got {norm}"
        );
    }

    #[test]
    fn test_unicode_text() {
        let embedder = HashEmbedder::new(256);

        // Should handle unicode gracefully
        let embedding = embedder.embed_sync("café résumé naïve").unwrap();
        assert_eq!(embedding.len(), 256);

        // Normalized
        let norm: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-5,
            "L2 norm should be ~1.0, got {norm}"
        );
    }

    #[test]
    fn test_embedding_similarity() {
        let embedder = HashEmbedder::new(256);

        // Similar texts should have higher cosine similarity
        let emb_dog = embedder.embed_sync("the quick brown dog").unwrap();
        let emb_fox = embedder.embed_sync("the quick brown fox").unwrap();
        let emb_unrelated = embedder.embed_sync("quantum physics equations").unwrap();

        // Compute cosine similarity (dot product of normalized vectors)
        let sim_dog_fox: f32 = emb_dog.iter().zip(&emb_fox).map(|(a, b)| a * b).sum();
        let sim_dog_unrelated: f32 = emb_dog.iter().zip(&emb_unrelated).map(|(a, b)| a * b).sum();

        // Dog and fox should be more similar (share more tokens)
        assert!(
            sim_dog_fox > sim_dog_unrelated,
            "similar texts should have higher cosine similarity: dog_fox={sim_dog_fox}, dog_unrelated={sim_dog_unrelated}"
        );
    }

    #[test]
    fn test_sync_embedder_adapter_bridge() {
        use frankensearch::SyncEmbedderAdapter;

        let embedder = HashEmbedder::new(256);
        let adapted = SyncEmbedderAdapter(embedder);

        // The adapter implements frankensearch::Embedder (async trait)
        assert_eq!(frankensearch::Embedder::dimension(&adapted), 256);
        assert_eq!(frankensearch::Embedder::id(&adapted), "fnv1a-256");
        assert!(!frankensearch::Embedder::is_semantic(&adapted));
    }
}
