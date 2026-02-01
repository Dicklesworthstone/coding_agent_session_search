//! Background embedding worker for the daemon.
//!
//! Processes embedding jobs on a dedicated thread using sync primitives.
//! Adapted from xf's async worker to cass's sync daemon architecture.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};

use tracing::{debug, error, info, warn};

use crate::indexer::semantic::{EmbeddingInput, SemanticIndexer};
use crate::search::canonicalize::{canonicalize_for_embedding, content_hash};
use crate::search::vector_index::{VectorIndex, role_code_from_str};
use crate::storage::sqlite::SqliteStorage;

/// Configuration for a single embedding job.
#[derive(Debug, Clone)]
pub struct EmbeddingJobConfig {
    pub db_path: String,
    pub index_path: String,
    pub two_tier: bool,
    pub fast_model: Option<String>,
    pub quality_model: Option<String>,
}

/// Messages sent to the background worker.
#[derive(Debug)]
pub enum WorkerMessage {
    /// Submit a new embedding job.
    Submit(EmbeddingJobConfig),
    /// Cancel jobs for a db_path, optionally filtered by model_id.
    Cancel {
        db_path: String,
        model_id: Option<String>,
    },
    /// Shut down the worker thread.
    Shutdown,
}

/// Handle for sending messages to the background worker.
#[derive(Clone)]
pub struct EmbeddingWorkerHandle {
    sender: Sender<WorkerMessage>,
}

impl EmbeddingWorkerHandle {
    /// Submit an embedding job to the worker.
    pub fn submit(&self, config: EmbeddingJobConfig) -> Result<(), String> {
        self.sender
            .send(WorkerMessage::Submit(config))
            .map_err(|e| format!("worker channel closed: {e}"))
    }

    /// Cancel embedding jobs for a db_path.
    pub fn cancel(&self, db_path: String, model_id: Option<String>) -> Result<(), String> {
        self.sender
            .send(WorkerMessage::Cancel { db_path, model_id })
            .map_err(|e| format!("worker channel closed: {e}"))
    }

    /// Request the worker to shut down.
    pub fn shutdown(&self) -> Result<(), String> {
        self.sender
            .send(WorkerMessage::Shutdown)
            .map_err(|e| format!("worker channel closed: {e}"))
    }
}

/// Background embedding worker that processes jobs on a dedicated thread.
pub struct EmbeddingWorker {
    receiver: Receiver<WorkerMessage>,
    cancel_flag: Arc<AtomicBool>,
}

impl EmbeddingWorker {
    /// Create a new worker and its handle.
    pub fn new() -> (Self, EmbeddingWorkerHandle) {
        let (sender, receiver) = std::sync::mpsc::channel();
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let worker = Self {
            receiver,
            cancel_flag,
        };
        let handle = EmbeddingWorkerHandle { sender };
        (worker, handle)
    }

    /// Run the worker loop (blocking). Call from a spawned thread.
    pub fn run(self) {
        info!("Embedding worker started");
        while let Ok(msg) = self.receiver.recv() {
            match msg {
                WorkerMessage::Submit(config) => {
                    self.cancel_flag.store(false, Ordering::SeqCst);
                    info!(db_path = %config.db_path, two_tier = config.two_tier, "Processing embedding job");
                    if let Err(e) = self.process_job(&config) {
                        error!(db_path = %config.db_path, error = %e, "Embedding job failed");
                    }
                }
                WorkerMessage::Cancel { db_path, model_id } => {
                    info!(%db_path, ?model_id, "Cancelling embedding jobs");
                    self.cancel_flag.store(true, Ordering::SeqCst);
                    // Also cancel in the database
                    if let Err(e) = Self::cancel_in_db(&db_path, model_id.as_deref()) {
                        warn!(%db_path, error = %e, "Failed to cancel jobs in database");
                    }
                }
                WorkerMessage::Shutdown => {
                    info!("Embedding worker shutting down");
                    break;
                }
            }
        }
        info!("Embedding worker stopped");
    }

    /// Cancel jobs in the database.
    fn cancel_in_db(db_path: &str, model_id: Option<&str>) -> anyhow::Result<()> {
        let storage = SqliteStorage::open(Path::new(db_path))?;
        storage.cancel_embedding_jobs(db_path, model_id)?;
        Ok(())
    }

    /// Process a single embedding job.
    fn process_job(&self, config: &EmbeddingJobConfig) -> anyhow::Result<()> {
        let db_path = Path::new(&config.db_path);
        let index_path = Path::new(&config.index_path);

        // Open storage and fetch messages
        let storage = SqliteStorage::open(db_path)?;
        let messages = storage.fetch_messages_for_embedding()?;
        let total_docs = i64::try_from(messages.len()).unwrap_or(i64::MAX);

        if total_docs == 0 {
            info!(db_path = %config.db_path, "No messages to embed");
            return Ok(());
        }

        info!(
            db_path = %config.db_path,
            total_docs,
            two_tier = config.two_tier,
            "Found messages to embed"
        );

        // Determine which passes to run
        let passes = self.build_passes(config);

        for (model_name, use_semantic) in &passes {
            if self.cancel_flag.load(Ordering::SeqCst) {
                info!("Embedding job cancelled");
                return Ok(());
            }

            let job_id = storage.upsert_embedding_job(&config.db_path, model_name, total_docs)?;
            storage.start_embedding_job(job_id)?;

            match self.generate_embeddings_and_save(
                &storage,
                &messages,
                model_name,
                *use_semantic,
                job_id,
                index_path,
            ) {
                Ok(()) => {
                    storage.complete_embedding_job(job_id)?;
                    info!(model = model_name, "Embedding pass completed");
                }
                Err(e) => {
                    let err_msg = format!("{e:#}");
                    storage.fail_embedding_job(job_id, &err_msg)?;
                    warn!(model = model_name, error = %e, "Embedding pass failed");
                }
            }
        }

        Ok(())
    }

    /// Determine the embedding passes to run based on config.
    fn build_passes(&self, config: &EmbeddingJobConfig) -> Vec<(String, bool)> {
        let mut passes = Vec::new();

        if config.two_tier {
            // Fast hash pass
            let fast = config
                .fast_model
                .clone()
                .unwrap_or_else(|| "hash".to_string());
            passes.push((fast, false));

            // Quality semantic pass
            let quality = config
                .quality_model
                .clone()
                .unwrap_or_else(|| "minilm".to_string());
            passes.push((quality, true));
        } else {
            // Single pass with best available
            let model = config
                .quality_model
                .clone()
                .or_else(|| config.fast_model.clone())
                .unwrap_or_else(|| "hash".to_string());
            let is_semantic = model != "hash";
            passes.push((model, is_semantic));
        }

        passes
    }

    /// Generate embeddings for messages and save the vector index.
    fn generate_embeddings_and_save(
        &self,
        storage: &SqliteStorage,
        messages: &[crate::storage::sqlite::MessageForEmbedding],
        model_name: &str,
        use_semantic: bool,
        job_id: i64,
        index_path: &Path,
    ) -> anyhow::Result<()> {
        // Load existing index to check for unchanged documents
        let existing_hashes = self.load_existing_hashes(index_path, model_name);

        // Prepare inputs, skipping unchanged documents
        let mut inputs: Vec<EmbeddingInput> = Vec::new();
        let mut skipped_count = 0usize;
        let mut completed = 0i64;

        for msg in messages {
            if self.cancel_flag.load(Ordering::SeqCst) {
                return Err(anyhow::anyhow!("job cancelled"));
            }

            let canonical = canonicalize_for_embedding(&msg.content);
            if canonical.is_empty() {
                completed += 1;
                continue;
            }

            let hash = content_hash(&canonical);
            let role = role_code_from_str(&msg.role).unwrap_or(0);

            // Check if this document is unchanged - skip re-embedding if hash matches
            if let Some(existing_hash) = existing_hashes.get(&(msg.message_id as u64))
                && *existing_hash == hash
            {
                skipped_count += 1;
                completed += 1;
                continue;
            }

            // Use saturating casts to handle edge cases gracefully
            let message_id = u64::try_from(msg.message_id).unwrap_or(0);
            let agent_id = u32::try_from(msg.agent_id).unwrap_or(0);
            let workspace_id = u32::try_from(msg.workspace_id.unwrap_or(0)).unwrap_or(0);

            inputs.push(EmbeddingInput {
                message_id,
                created_at_ms: msg.created_at.unwrap_or(0),
                agent_id,
                workspace_id,
                source_id: msg.source_id_hash,
                role,
                chunk_idx: 0,
                content: canonical,
            });

            completed += 1;
            if completed % 100 == 0 {
                let _ = storage.update_job_progress(job_id, completed);
                debug!(job_id, completed, "Embedding progress");
            }
        }

        if inputs.is_empty() {
            info!(
                model = model_name,
                skipped = skipped_count,
                "No documents to embed - all unchanged"
            );
            return Ok(());
        }

        info!(
            model = model_name,
            input_count = inputs.len(),
            skipped = skipped_count,
            "Embedding documents"
        );

        // Create the appropriate embedder/indexer
        let indexer = if use_semantic {
            SemanticIndexer::new("fastembed", Some(index_path))?
        } else {
            SemanticIndexer::new("hash", None)?
        };

        // Embed messages
        let embedded = indexer.embed_messages(&inputs)?;

        // Update final progress
        let _ = storage.update_job_progress(job_id, messages.len() as i64);

        // Build and save vector index
        let index = indexer.build_index(embedded)?;
        let save_path = indexer.save_index(&index, index_path)?;

        info!(
            model = model_name,
            path = %save_path.display(),
            count = inputs.len(),
            "Saved vector index"
        );

        Ok(())
    }

    /// Load content hashes from an existing vector index for dedup.
    fn load_existing_hashes(&self, index_path: &Path, model_name: &str) -> HashMap<u64, [u8; 32]> {
        let embedder_id = match model_name {
            "hash" => "fnv1a-384",
            "minilm" => "minilm-384",
            other => other,
        };

        let cvvi_path = index_path
            .join("vector_index")
            .join(format!("index-{embedder_id}.cvvi"));

        if !cvvi_path.exists() {
            return HashMap::new();
        }

        match VectorIndex::load(&cvvi_path) {
            Ok(index) => {
                let mut hashes = HashMap::new();
                for row in index.rows() {
                    hashes.insert(row.message_id, row.content_hash);
                }
                debug!(
                    path = %cvvi_path.display(),
                    count = hashes.len(),
                    "Loaded existing hashes for dedup"
                );
                hashes
            }
            Err(e) => {
                warn!(
                    path = %cvvi_path.display(),
                    error = %e,
                    "Failed to load existing index for dedup"
                );
                HashMap::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_worker_handle_clone() {
        let (_worker, handle) = EmbeddingWorker::new();
        let handle2 = handle.clone();
        // Both handles should be able to send
        assert!(handle.shutdown().is_ok());
        // Second handle will fail since receiver got Shutdown and loop ended
        // But the channel itself is still open until worker drops
        drop(handle2);
    }

    #[test]
    fn test_job_config() {
        let config = EmbeddingJobConfig {
            db_path: "/tmp/test.db".to_string(),
            index_path: "/tmp/test_index".to_string(),
            two_tier: true,
            fast_model: Some("hash".to_string()),
            quality_model: Some("minilm".to_string()),
        };
        assert!(config.two_tier);
        assert_eq!(config.fast_model.as_deref(), Some("hash"));
        assert_eq!(config.quality_model.as_deref(), Some("minilm"));
    }

    #[test]
    fn test_build_passes_single() {
        let (_worker, _handle) = EmbeddingWorker::new();
        let config = EmbeddingJobConfig {
            db_path: String::new(),
            index_path: String::new(),
            two_tier: false,
            fast_model: None,
            quality_model: Some("minilm".to_string()),
        };
        let passes = _worker.build_passes(&config);
        assert_eq!(passes.len(), 1);
        assert_eq!(passes[0].0, "minilm");
        assert!(passes[0].1); // semantic
    }

    #[test]
    fn test_build_passes_two_tier() {
        let (_worker, _handle) = EmbeddingWorker::new();
        let config = EmbeddingJobConfig {
            db_path: String::new(),
            index_path: String::new(),
            two_tier: true,
            fast_model: Some("hash".to_string()),
            quality_model: Some("minilm".to_string()),
        };
        let passes = _worker.build_passes(&config);
        assert_eq!(passes.len(), 2);
        assert_eq!(passes[0].0, "hash");
        assert!(!passes[0].1); // not semantic
        assert_eq!(passes[1].0, "minilm");
        assert!(passes[1].1); // semantic
    }

    #[test]
    fn test_build_passes_defaults() {
        let (_worker, _handle) = EmbeddingWorker::new();
        let config = EmbeddingJobConfig {
            db_path: String::new(),
            index_path: String::new(),
            two_tier: false,
            fast_model: None,
            quality_model: None,
        };
        let passes = _worker.build_passes(&config);
        assert_eq!(passes.len(), 1);
        assert_eq!(passes[0].0, "hash");
        assert!(!passes[0].1); // hash is not semantic
    }
}
