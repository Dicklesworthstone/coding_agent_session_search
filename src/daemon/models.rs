//! Model manager for lazy loading embedder and reranker models.
//!
//! This module provides lazy-loaded access to embedding and reranking models,
//! supporting graceful fallback when models are unavailable.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::RwLock;
use tracing::{info, warn};

use crate::search::embedder::{Embedder, EmbedderError, EmbedderResult};
use crate::search::fastembed_embedder::FastEmbedder;
use crate::search::fastembed_reranker::FastEmbedReranker;
use crate::search::hash_embedder::HashEmbedder;
use crate::search::reranker::{Reranker, RerankerError, RerankerResult};

/// Model manager that handles lazy loading of embedder and reranker models.
pub struct ModelManager {
    data_dir: PathBuf,
    embedder: RwLock<Option<Arc<dyn Embedder>>>,
    reranker: RwLock<Option<Arc<dyn Reranker>>>,
    embedder_name: RwLock<String>,
    reranker_name: RwLock<String>,
    fallback_embedder: Arc<HashEmbedder>,
}

impl ModelManager {
    /// Create a new model manager with the given data directory.
    pub fn new(data_dir: &Path) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
            embedder: RwLock::new(None),
            reranker: RwLock::new(None),
            embedder_name: RwLock::new("not-loaded".to_string()),
            reranker_name: RwLock::new("not-loaded".to_string()),
            fallback_embedder: Arc::new(HashEmbedder::new(384)),
        }
    }

    /// Check if any model is loaded and ready.
    pub fn is_ready(&self) -> bool {
        self.embedder.read().is_some()
    }

    /// Get the embedder ID.
    pub fn embedder_id(&self) -> String {
        self.embedder
            .read()
            .as_ref()
            .map(|e| e.id().to_string())
            .unwrap_or_else(|| "hash-384".to_string())
    }

    /// Get the embedder name.
    pub fn embedder_name(&self) -> String {
        self.embedder_name.read().clone()
    }

    /// Get the embedder dimension.
    pub fn embedder_dimension(&self) -> usize {
        self.embedder
            .read()
            .as_ref()
            .map(|e| e.dimension())
            .unwrap_or(384)
    }

    /// Check if embedder is loaded.
    pub fn embedder_loaded(&self) -> bool {
        self.embedder.read().is_some()
    }

    /// Get the reranker ID.
    pub fn reranker_id(&self) -> String {
        self.reranker
            .read()
            .as_ref()
            .map(|r| r.id().to_string())
            .unwrap_or_else(|| "none".to_string())
    }

    /// Get the reranker name.
    pub fn reranker_name(&self) -> String {
        self.reranker_name.read().clone()
    }

    /// Check if reranker is loaded.
    pub fn reranker_loaded(&self) -> bool {
        self.reranker.read().is_some()
    }

    /// Pre-warm the embedder by loading it.
    pub fn warm_embedder(&self) -> EmbedderResult<()> {
        if self.embedder.read().is_some() {
            return Ok(());
        }

        let model_dir = FastEmbedder::default_model_dir(&self.data_dir);
        info!(model_dir = %model_dir.display(), "Loading embedder");

        match FastEmbedder::load_from_dir(&model_dir) {
            Ok(embedder) => {
                let id = embedder.id().to_string();
                let dimension = embedder.dimension();
                *self.embedder.write() = Some(Arc::new(embedder));
                *self.embedder_name.write() = "MiniLM-L6-v2".to_string();
                info!(id = %id, dimension = dimension, "Embedder loaded");
                Ok(())
            }
            Err(e) => {
                warn!(error = %e, "Failed to load embedder, using hash fallback");
                *self.embedder.write() = Some(self.fallback_embedder.clone());
                *self.embedder_name.write() = "hash-fallback".to_string();
                // Return Ok since we have a fallback
                Ok(())
            }
        }
    }

    /// Pre-warm the reranker by loading it.
    pub fn warm_reranker(&self) -> RerankerResult<()> {
        if self.reranker.read().is_some() {
            return Ok(());
        }

        let model_dir = FastEmbedReranker::default_model_dir(&self.data_dir);
        info!(model_dir = %model_dir.display(), "Loading reranker");

        match FastEmbedReranker::load_from_dir(&model_dir) {
            Ok(reranker) => {
                let id = reranker.id().to_string();
                *self.reranker.write() = Some(Arc::new(reranker));
                *self.reranker_name.write() = "ms-marco-MiniLM-L-6-v2".to_string();
                info!(id = %id, "Reranker loaded");
                Ok(())
            }
            Err(e) => {
                warn!(error = %e, "Failed to load reranker, reranking unavailable");
                Err(e)
            }
        }
    }

    /// Embed a batch of texts.
    pub fn embed_batch(&self, texts: &[String]) -> EmbedderResult<Vec<Vec<f32>>> {
        // Ensure embedder is loaded
        if self.embedder.read().is_none() {
            self.warm_embedder()?;
        }

        let embedder = self.embedder.read();
        let embedder = embedder
            .as_ref()
            .ok_or_else(|| EmbedderError::Unavailable("embedder not loaded".to_string()))?;

        // Convert to &str slice for the batch call
        let text_refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
        embedder.embed_batch(&text_refs)
    }

    /// Embed a single text.
    pub fn embed(&self, text: &str) -> EmbedderResult<Vec<f32>> {
        // Ensure embedder is loaded
        if self.embedder.read().is_none() {
            self.warm_embedder()?;
        }

        let embedder = self.embedder.read();
        let embedder = embedder
            .as_ref()
            .ok_or_else(|| EmbedderError::Unavailable("embedder not loaded".to_string()))?;

        embedder.embed(text)
    }

    /// Rerank documents against a query.
    pub fn rerank(&self, query: &str, documents: &[String]) -> RerankerResult<Vec<f32>> {
        // Ensure reranker is loaded
        if self.reranker.read().is_none() {
            self.warm_reranker()?;
        }

        let reranker = self.reranker.read();
        let reranker = reranker
            .as_ref()
            .ok_or_else(|| RerankerError::Unavailable("reranker not loaded".to_string()))?;

        // Convert to &str slice
        let doc_refs: Vec<&str> = documents.iter().map(|s| s.as_str()).collect();
        reranker.rerank(query, &doc_refs)
    }

    /// Unload all models to free memory.
    pub fn unload_all(&self) {
        *self.embedder.write() = None;
        *self.reranker.write() = None;
        *self.embedder_name.write() = "not-loaded".to_string();
        *self.reranker_name.write() = "not-loaded".to_string();
        info!("All models unloaded");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_data_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
    }

    fn model_fixture_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/models/xenova-paraphrase-minilm-l3-v2-int8")
    }

    #[test]
    fn test_model_manager_creation() {
        let manager = ModelManager::new(&test_data_dir());
        assert!(!manager.is_ready());
        assert!(!manager.embedder_loaded());
        assert!(!manager.reranker_loaded());
    }

    #[test]
    fn test_embedder_fallback_on_missing_model() {
        // Use a directory without models
        let manager = ModelManager::new(&PathBuf::from("/tmp/nonexistent"));

        // Should succeed with fallback
        let result = manager.warm_embedder();
        assert!(result.is_ok());

        // Should be using hash fallback
        assert!(manager.embedder_loaded());
        assert_eq!(manager.embedder_name(), "hash-fallback");
    }

    #[test]
    fn test_embedder_dimension() {
        let manager = ModelManager::new(&test_data_dir());
        // Before loading, should return default dimension
        assert_eq!(manager.embedder_dimension(), 384);
    }

    #[test]
    fn test_unload_all() {
        let manager = ModelManager::new(&test_data_dir());
        let _ = manager.warm_embedder();

        assert!(manager.embedder_loaded());

        manager.unload_all();

        assert!(!manager.embedder_loaded());
        assert!(!manager.reranker_loaded());
    }

    #[test]
    fn test_embed_with_fallback() {
        let manager = ModelManager::new(&PathBuf::from("/tmp/nonexistent"));

        // Should work with fallback
        let result = manager.embed("test text");
        assert!(result.is_ok());

        let embedding = result.unwrap();
        assert_eq!(embedding.len(), 384);
    }
}
