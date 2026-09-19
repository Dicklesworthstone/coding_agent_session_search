//! Model manager for lazy loading embedder and reranker models.
//!
//! Native loading is single-flight but never holds the published-state lock
//! used by health/status. Queries borrow an immutable model owner; publishing
//! or unloading a model cannot invalidate an already-running inference call.

mod slot;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tracing::{info, warn};

use super::protocol::ModelInfo;
use crate::search::embedder::{Embedder, EmbedderError, EmbedderResult};
use crate::search::fastembed_embedder::FastEmbedder;
use crate::search::fastembed_reranker::FastEmbedReranker;
use crate::search::reranker::{Reranker, RerankerError, RerankerResult, rerank_texts};
use frankensearch::ModelCategory;
use frankensearch::core::EmbeddingIdentityBundleV1;
use slot::ModelSlot;

/// Lazy model owners with independently readable publication state.
pub struct ModelManager {
    data_dir: PathBuf,
    embedder_registry_name: String,
    embedder: ModelSlot<dyn Embedder>,
    reranker: ModelSlot<dyn Reranker>,
}

impl ModelManager {
    /// Create a new model manager with the given data directory.
    pub fn new(data_dir: &Path) -> Self {
        let policy = crate::search::policy::SemanticPolicy::resolve(
            &crate::search::policy::CliSemanticOverrides::default(),
        );
        Self::new_for_embedder(data_dir, &policy.quality_tier_embedder)
    }

    fn new_for_embedder(data_dir: &Path, embedder_registry_name: &str) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
            embedder_registry_name: FastEmbedder::canonical_name(embedder_registry_name)
                .unwrap_or(embedder_registry_name)
                .to_owned(),
            embedder: ModelSlot::new(),
            reranker: ModelSlot::new(),
        }
    }

    /// Readiness never waits for model loading or native inference.
    pub fn is_ready(&self) -> bool {
        self.embedder.get().is_some()
    }

    pub fn embedder_id(&self) -> String {
        self.embedder
            .get()
            .map(|model| model.id().to_owned())
            .unwrap_or_else(|| "not-loaded".to_owned())
    }

    pub fn embedder_name(&self) -> String {
        self.embedder.snapshot().name
    }

    pub fn embedder_dimension(&self) -> usize {
        // Preserve the historical unloaded getter contract. Status instead
        // reports None until the native producer has actually been loaded.
        self.embedder
            .get()
            .map(|model| model.dimension())
            .unwrap_or(384)
    }

    pub fn embedder_loaded(&self) -> bool {
        self.embedder.get().is_some()
    }

    /// Exact immutable identity of the loaded producer, never a model-name
    /// approximation or a hash substitute. Loading remains explicit here.
    pub fn embedder_attestation_identity(
        &self,
    ) -> EmbedderResult<(EmbeddingIdentityBundleV1, ModelCategory)> {
        self.warm_embedder()?;
        let model = self
            .embedder
            .get()
            .ok_or_else(|| EmbedderError::EmbedderUnavailable {
                model: "unknown".to_owned(),
                reason: "embedder not loaded".to_owned(),
            })?;
        let identity = model.identity()?.clone();
        identity.validate()?;
        Ok((identity, model.category()))
    }

    pub fn reranker_id(&self) -> String {
        self.reranker
            .get()
            .map(|model| model.id().to_owned())
            .unwrap_or_else(|| "none".to_owned())
    }

    pub fn reranker_name(&self) -> String {
        self.reranker.snapshot().name
    }

    pub fn reranker_loaded(&self) -> bool {
        self.reranker.get().is_some()
    }

    /// Each status record is built from one coherent publication snapshot.
    pub fn embedder_info(&self) -> ModelInfo {
        let snapshot = self.embedder.snapshot();
        ModelInfo {
            id: snapshot
                .model
                .as_ref()
                .map(|model| model.id().to_owned())
                .unwrap_or_else(|| "not-loaded".to_owned()),
            name: snapshot.name,
            dimension: snapshot.model.as_ref().map(|model| model.dimension()),
            loaded: snapshot.model.is_some(),
            memory_bytes: 0,
        }
    }

    pub fn reranker_info(&self) -> ModelInfo {
        let snapshot = self.reranker.snapshot();
        ModelInfo {
            id: snapshot
                .model
                .as_ref()
                .map(|model| model.id().to_owned())
                .unwrap_or_else(|| "none".to_owned()),
            name: snapshot.name,
            dimension: None,
            loaded: snapshot.model.is_some(),
            memory_bytes: 0,
        }
    }

    /// Load one native embedder. Only the loader gate is held during I/O.
    pub fn warm_embedder(&self) -> EmbedderResult<()> {
        self.embedder.load_with(|| {
            // Check selection before model-directory overrides. Installed
            // MiniLM bytes must never enable a disabled quality tier.
            if FastEmbedder::canonical_name(&self.embedder_registry_name).is_none() {
                return Err(EmbedderError::EmbedderUnavailable {
                    model: self.embedder_registry_name.clone(),
                    reason:
                        "selected quality model is disabled or unsupported by the native daemon"
                            .to_owned(),
                });
            }
            let model_dir =
                FastEmbedder::runtime_model_dir_for(&self.data_dir, &self.embedder_registry_name)
                    .ok_or_else(|| EmbedderError::EmbedderUnavailable {
                    model: self.embedder_registry_name.clone(),
                    reason: "registered embedder has no model directory mapping".to_owned(),
                })?;
            info!(embedder = self.embedder_registry_name, model_dir = %model_dir.display(),
                "Loading embedder");
            let model = FastEmbedder::load_by_name(&self.data_dir, &self.embedder_registry_name)
                .map_err(|error| {
                    warn!(registry_name = self.embedder_registry_name, error = %error,
                        "Failed to load semantic embedder");
                    error
                })?;
            let name = model.model_name().to_owned();
            info!(
                registry_name = self.embedder_registry_name,
                id = model.id(),
                dimension = model.dimension(),
                "Embedder loaded"
            );
            Ok((Arc::new(model) as Arc<dyn Embedder>, name))
        })
    }

    pub fn warm_reranker(&self) -> RerankerResult<()> {
        self.reranker.load_with(|| {
            let model_dir = FastEmbedReranker::default_model_dir(&self.data_dir);
            info!(model_dir = %model_dir.display(), "Loading reranker");
            let model = FastEmbedReranker::load_from_dir(&model_dir).map_err(|error| {
                warn!(error = %error, "Failed to load reranker, reranking unavailable");
                error
            })?;
            info!(id = model.id(), "Reranker loaded");
            Ok((
                Arc::new(model) as Arc<dyn Reranker>,
                "ms-marco-MiniLM-L-6-v2".to_owned(),
            ))
        })
    }

    pub fn embed_batch(&self, texts: &[String]) -> EmbedderResult<Vec<Vec<f32>>> {
        self.warm_embedder()?;
        let model = self
            .embedder
            .get()
            .ok_or_else(|| EmbedderError::EmbedderUnavailable {
                model: "unknown".to_owned(),
                reason: "embedder not loaded".to_owned(),
            })?;
        let texts: Vec<&str> = texts.iter().map(String::as_str).collect();
        model.embed_batch_sync(&texts)
    }

    pub fn embed(&self, text: &str) -> EmbedderResult<Vec<f32>> {
        self.warm_embedder()?;
        let model = self
            .embedder
            .get()
            .ok_or_else(|| EmbedderError::EmbedderUnavailable {
                model: "unknown".to_owned(),
                reason: "embedder not loaded".to_owned(),
            })?;
        model.embed_sync(text)
    }

    pub fn rerank(&self, query: &str, documents: &[String]) -> RerankerResult<Vec<f32>> {
        self.warm_reranker()?;
        let model = self
            .reranker
            .get()
            .ok_or_else(|| RerankerError::RerankerUnavailable {
                model: "reranker".to_owned(),
            })?;
        let documents: Vec<&str> = documents.iter().map(String::as_str).collect();
        rerank_texts(model.as_ref(), query, &documents)
    }

    /// Remove published owners; borrowed models finish safely and release their
    /// memory when the last in-flight request drops them. A pending load cannot
    /// republish after its slot has been unloaded.
    pub fn unload_all(&self) {
        self.embedder.unload();
        self.reranker.unload();
        info!("All models unloaded");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_data_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
    }

    #[allow(dead_code)]
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
    fn multilingual_model_manager_keeps_an_explicit_distinct_registry_selection() {
        let manager = ModelManager::new_for_embedder(&test_data_dir(), "multilingual-minilm");
        assert_eq!(manager.embedder_registry_name, "multilingual-minilm");
        assert_eq!(manager.embedder_name(), "not-loaded");
        assert_eq!(manager.embedder_id(), "not-loaded");
    }

    #[test]
    fn model_manager_canonicalizes_supported_quality_aliases() {
        for (selected, expected) in [
            ("all-minilm-l6-v2", "minilm"),
            ("multilingual", "multilingual-minilm"),
        ] {
            let manager = ModelManager::new_for_embedder(&test_data_dir(), selected);
            assert_eq!(manager.embedder_registry_name, expected);
        }
    }

    #[test]
    fn unsupported_quality_selection_never_loads_or_attests_a_substitute()
    -> Result<(), Box<dyn std::error::Error>> {
        const CHILD_SELECTION: &str = "CASS_DAEMON_MODEL_SELECTION_TEST_CHILD";
        let data_dir = tempfile::tempdir()?;
        if let Ok(selected) = dotenvy::var(CHILD_SELECTION) {
            let manager = ModelManager::new(data_dir.path());
            assert_eq!(manager.embedder_registry_name, selected);
            for result in [
                manager.warm_embedder(),
                manager.embed("a real daemon embedding request").map(|_| ()),
                manager.embedder_attestation_identity().map(|_| ()),
            ] {
                match result {
                    Err(EmbedderError::EmbedderUnavailable { model, reason }) => {
                        assert_eq!(model, selected);
                        assert!(reason.contains("disabled or unsupported"));
                    }
                    other => panic!("expected explicit selection refusal, got {other:?}"),
                }
            }
            assert!(!manager.is_ready());
            assert!(!manager.embedder_loaded());
            assert_eq!(manager.embedder_id(), "not-loaded");
            assert_eq!(manager.embedder_name(), "load-failed");
            assert_eq!(std::fs::read_dir(data_dir.path())?.count(), 0);
            return Ok(());
        }

        for selected in ["hash", "unknown-quality-model"] {
            let output = std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "daemon::models::tests::unsupported_quality_selection_never_loads_or_attests_a_substitute",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(CHILD_SELECTION, selected)
                .env("CASS_SEMANTIC_EMBEDDER", selected)
                .env("FRANKENSEARCH_MODEL_DIR", data_dir.path())
                .current_dir(data_dir.path())
                .output()?;
            assert!(
                output.status.success(),
                "selection {selected} failed: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"),
                "the exact child regression must actually execute"
            );
            assert_eq!(std::fs::read_dir(data_dir.path())?.count(), 0);
        }
        Ok(())
    }

    #[test]
    fn test_missing_model_is_reported_without_hash_substitution()
    -> Result<(), Box<dyn std::error::Error>> {
        let empty_data_dir = tempfile::tempdir()?;
        let manager = ModelManager::new(empty_data_dir.path());

        let result = manager.warm_embedder();
        assert!(result.is_err());
        assert!(!manager.embedder_loaded());
        assert_eq!(manager.embedder_id(), "not-loaded");
        assert_eq!(manager.embedder_name(), "load-failed");
        Ok(())
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
        assert_eq!(manager.embedder_name(), "load-failed");

        manager.unload_all();

        assert!(!manager.embedder_loaded());
        assert!(!manager.reranker_loaded());
        assert_eq!(manager.embedder_name(), "not-loaded");
    }

    #[test]
    fn test_embed_reports_missing_model() -> Result<(), Box<dyn std::error::Error>> {
        let empty_data_dir = tempfile::tempdir()?;
        let manager = ModelManager::new(empty_data_dir.path());

        let result = manager.embed("test text");
        assert!(result.is_err());
        assert!(!manager.embedder_loaded());
        Ok(())
    }
}

#[cfg(test)]
mod responsiveness_tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    fn check_loading_status(embedder: bool) {
        let temp = tempfile::tempdir().unwrap();
        let manager = Arc::new(ModelManager::new_for_embedder(temp.path(), "minilm"));
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let loading = Arc::clone(&manager);
        let loader = std::thread::spawn(move || {
            if embedder {
                loading
                    .embedder
                    .load_with(|| {
                        started_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                        Err::<_, EmbedderError>(EmbedderError::EmbedderUnavailable {
                            model: "fixture".to_owned(),
                            reason: "injected stalled load".to_owned(),
                        })
                    })
                    .is_err()
            } else {
                loading
                    .reranker
                    .load_with(|| {
                        started_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                        Err::<_, RerankerError>(RerankerError::RerankerUnavailable {
                            model: "fixture".to_owned(),
                        })
                    })
                    .is_err()
            }
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let inspecting = Arc::clone(&manager);
        let (tx, rx) = mpsc::channel();
        let observer = std::thread::spawn(move || {
            tx.send((
                inspecting.is_ready(),
                inspecting.embedder_info(),
                inspecting.reranker_info(),
            ))
            .unwrap();
        });
        let result = rx.recv_timeout(Duration::from_secs(1));
        release_tx.send(()).unwrap();
        assert!(loader.join().unwrap());
        observer.join().unwrap();
        let (ready, embedding, reranking) = result.expect("status waited for a model loader");
        assert!(!ready);
        assert!(!embedding.loaded);
        assert!(!reranking.loaded);
        assert_eq!(embedding.dimension, None);
        assert_eq!(
            if embedder {
                embedding.name
            } else {
                reranking.name
            },
            "loading"
        );
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
    }

    #[test]
    fn public_status_and_health_do_not_wait_for_embedder_loading() {
        check_loading_status(true);
    }

    #[test]
    fn public_status_and_health_do_not_wait_for_reranker_loading() {
        check_loading_status(false);
    }

    #[test]
    fn unloaded_status_does_not_invent_a_producer_dimension() {
        let temp = tempfile::tempdir().unwrap();
        let manager = ModelManager::new_for_embedder(temp.path(), "minilm");
        let info = manager.embedder_info();
        assert!(!info.loaded);
        assert_eq!(info.dimension, None);
        assert_eq!(info.id, "not-loaded");
        assert_eq!(info.name, "not-loaded");
        assert_eq!(
            manager.embedder_dimension(),
            384,
            "legacy getter remains compatible"
        );
    }
}
