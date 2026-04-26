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

use crate::indexer::semantic::{
    EmbeddingInput, SemanticIndexer, message_id_from_db, saturating_u32_from_i64,
};
use crate::search::canonicalize::{canonicalize_for_embedding, content_hash};
use crate::search::fastembed_embedder::FastEmbedder;
use crate::search::vector_index::{
    VectorIndex, parse_semantic_doc_id, role_code_from_str, vector_index_path,
};
use crate::storage::sqlite::FrankenStorage;

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
    /// Shared cancel flag — set directly from the handle so cancellation
    /// takes effect even while `process_job` is running on the worker thread.
    cancel_flag: Arc<AtomicBool>,
}

impl EmbeddingWorkerHandle {
    /// Submit an embedding job to the worker.
    pub fn submit(&self, config: EmbeddingJobConfig) -> Result<(), String> {
        self.sender
            .send(WorkerMessage::Submit(config))
            .map_err(|e| format!("worker channel closed: {e}"))
    }

    /// Cancel embedding jobs for a db_path.
    ///
    /// Sets the cancel flag directly (so the running job sees it immediately)
    /// AND sends a Cancel message for database-level cleanup.
    pub fn cancel(&self, db_path: String, model_id: Option<String>) -> Result<(), String> {
        self.cancel_flag.store(true, Ordering::SeqCst);
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum WorkerEmbedderKind {
    Hash,
    FastEmbed {
        model_name: String,
        embedder_id: String,
    },
}

fn resolve_embedder_kind(
    model_name: &str,
    use_semantic: bool,
) -> anyhow::Result<WorkerEmbedderKind> {
    if !use_semantic
        || model_name.eq_ignore_ascii_case("hash")
        || model_name.eq_ignore_ascii_case("fnv1a-384")
    {
        return Ok(WorkerEmbedderKind::Hash);
    }

    let normalized_name = match model_name.to_ascii_lowercase().as_str() {
        "fastembed" | "minilm" | "minilm-384" | "all-minilm-l6-v2" => "minilm",
        "snowflake-arctic-s" | "snowflake-arctic-s-384" | "snowflake-arctic-embed-s" => {
            "snowflake-arctic-s"
        }
        "nomic-embed" | "nomic-embed-768" | "nomic-embed-text-v1.5" => "nomic-embed",
        _ => {
            anyhow::bail!(
                "unsupported semantic model '{model_name}' for daemon embedding worker; supported: minilm, snowflake-arctic-s, nomic-embed"
            );
        }
    };

    let config = FastEmbedder::config_for(normalized_name).ok_or_else(|| {
        anyhow::anyhow!("missing FastEmbedder config for registered model '{normalized_name}'")
    })?;
    Ok(WorkerEmbedderKind::FastEmbed {
        model_name: normalized_name.to_string(),
        embedder_id: config.embedder_id,
    })
}

impl EmbeddingWorker {
    /// Create a new worker and its handle.
    pub fn new() -> (Self, EmbeddingWorkerHandle) {
        let (sender, receiver) = std::sync::mpsc::channel();
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let handle = EmbeddingWorkerHandle {
            sender,
            cancel_flag: Arc::clone(&cancel_flag),
        };
        let worker = Self {
            receiver,
            cancel_flag,
        };
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
                    // The cancel_flag is already set by the handle (so the running
                    // job sees it immediately). This handler performs DB cleanup.
                    info!(%db_path, ?model_id, "Processing cancel — flag already set by handle");
                    // Cancel in the database
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
        let storage = FrankenStorage::open(Path::new(db_path))?;
        storage.cancel_embedding_jobs(db_path, model_id)?;
        Ok(())
    }

    /// Process a single embedding job.
    fn process_job(&self, config: &EmbeddingJobConfig) -> anyhow::Result<()> {
        let db_path = Path::new(&config.db_path);
        let index_path = Path::new(&config.index_path);

        // Open storage and fetch messages
        let storage = FrankenStorage::open(db_path)?;
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
        storage: &FrankenStorage,
        messages: &[crate::storage::sqlite::MessageForEmbedding],
        model_name: &str,
        use_semantic: bool,
        job_id: i64,
        index_path: &Path,
    ) -> anyhow::Result<()> {
        let embedder_kind = resolve_embedder_kind(model_name, use_semantic)?;

        // Load existing index to check for unchanged documents
        let existing_hashes = self.load_existing_hashes(index_path, &embedder_kind);

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

            // Invalid/negative IDs indicate corrupted data; skip rather than collapsing to 0.
            let Some(message_id) = message_id_from_db(msg.message_id) else {
                warn!(
                    raw_message_id = msg.message_id,
                    "Skipping message with out-of-range id during embedding"
                );
                completed += 1;
                continue;
            };

            // Check if this document is unchanged - skip re-embedding if hash matches
            if let Some(existing_hash) = existing_hashes.get(&message_id)
                && *existing_hash == hash
            {
                skipped_count += 1;
                completed += 1;
                continue;
            }

            // Clamp to a stable range instead of silently wrapping/failing.
            let agent_id = saturating_u32_from_i64(msg.agent_id);
            let workspace_id = saturating_u32_from_i64(msg.workspace_id.unwrap_or(0));

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
            let final_completed = i64::try_from(messages.len()).unwrap_or(i64::MAX);
            let _ = storage.update_job_progress(job_id, final_completed);
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
        let indexer = match embedder_kind {
            WorkerEmbedderKind::Hash => SemanticIndexer::new("hash", None)?,
            WorkerEmbedderKind::FastEmbed { ref model_name, .. } => {
                SemanticIndexer::new(model_name, Some(index_path))?
            }
        };

        // Embed messages
        let embedded = indexer.embed_messages(&inputs)?;

        // Update final progress
        let final_completed = i64::try_from(messages.len()).unwrap_or(i64::MAX);
        let _ = storage.update_job_progress(job_id, final_completed);

        // Append to existing vector index, or create a new one if none exists.
        // Using append_to_index preserves previously-indexed unchanged documents
        // that were skipped by the dedup check above.
        let save_path = vector_index_path(index_path, indexer.embedder_id());
        if save_path.exists() {
            let appended = indexer.append_to_index(embedded, index_path)?;
            info!(appended, "Appended to existing vector index");
        } else {
            let _index = indexer.build_and_save_index(embedded, index_path)?;
        }

        info!(
            model = model_name,
            path = %save_path.display(),
            count = inputs.len(),
            "Saved vector index"
        );

        Ok(())
    }

    /// Load content hashes from an existing vector index for dedup.
    fn load_existing_hashes(
        &self,
        index_path: &Path,
        embedder_kind: &WorkerEmbedderKind,
    ) -> HashMap<u64, [u8; 32]> {
        let embedder_id = match embedder_kind {
            WorkerEmbedderKind::Hash => "fnv1a-384",
            WorkerEmbedderKind::FastEmbed { embedder_id, .. } => embedder_id.as_str(),
        };

        let fsvi_path = vector_index_path(index_path, embedder_id);

        if !fsvi_path.exists() {
            return HashMap::new();
        }

        match VectorIndex::open(&fsvi_path) {
            Ok(index) => {
                let mut hashes = HashMap::new();
                for idx in 0..index.record_count() {
                    let doc_id_str = match index.doc_id_at(idx) {
                        Ok(doc_id) => doc_id,
                        Err(_) => continue,
                    };

                    if let Some(parsed) = parse_semantic_doc_id(doc_id_str)
                        && let Some(hash) = parsed.content_hash
                    {
                        hashes.insert(parsed.message_id, hash);
                    }
                }
                debug!(
                    path = %fsvi_path.display(),
                    count = hashes.len(),
                    "Loaded existing hashes for dedup"
                );
                hashes
            }
            Err(e) => {
                warn!(
                    path = %fsvi_path.display(),
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

    #[test]
    fn test_message_id_from_db_rejects_negative_ids() {
        assert_eq!(message_id_from_db(-1), None);
        assert_eq!(message_id_from_db(0), Some(0));
        assert_eq!(message_id_from_db(42), Some(42));
    }

    #[test]
    fn test_saturating_u32_from_i64_clamps_bounds() {
        assert_eq!(saturating_u32_from_i64(-7), 0);
        assert_eq!(saturating_u32_from_i64(0), 0);
        assert_eq!(saturating_u32_from_i64(7), 7);
        assert_eq!(saturating_u32_from_i64(i64::from(u32::MAX) + 123), u32::MAX);
    }

    #[test]
    fn test_resolve_embedder_kind_hash_aliases() {
        assert_eq!(
            resolve_embedder_kind("hash", false).unwrap(),
            WorkerEmbedderKind::Hash
        );
        assert_eq!(
            resolve_embedder_kind("FNV1A-384", true).unwrap(),
            WorkerEmbedderKind::Hash
        );
    }

    /// `coding_agent_session_search-am69y`: pin the override-by-flag
    /// short-circuit at the top of `resolve_embedder_kind`. The
    /// `test_resolve_embedder_kind_hash_aliases` companion above
    /// exercises ("hash", false), but "hash" matches BOTH the
    /// `!use_semantic` branch AND the `eq_ignore_ascii_case("hash")`
    /// branch — so a regression that broke only the `!use_semantic`
    /// short-circuit would still be rescued by the name match and
    /// silently pass. This test pins the flag-only contract by
    /// passing semantic model names with `use_semantic=false`: every
    /// registered FastEmbedder name MUST resolve to `Hash` purely
    /// because the flag is false, regardless of name.
    #[test]
    fn test_resolve_embedder_kind_use_semantic_false_short_circuits_regardless_of_name() {
        for semantic_name in [
            "minilm",
            "minilm-384",
            "all-minilm-l6-v2",
            "fastembed",
            "snowflake-arctic-s",
            "snowflake-arctic-embed-s",
            "nomic-embed",
            "nomic-embed-text-v1.5",
            "MINILM",
        ] {
            assert_eq!(
                resolve_embedder_kind(semantic_name, false).unwrap(),
                WorkerEmbedderKind::Hash,
                "use_semantic=false MUST short-circuit to Hash regardless of model_name; \
                 regression on name {semantic_name:?} indicates the !use_semantic branch \
                 was bypassed"
            );
        }
    }

    #[test]
    fn test_resolve_embedder_kind_semantic_aliases() {
        assert_eq!(
            resolve_embedder_kind("minilm", true).unwrap(),
            WorkerEmbedderKind::FastEmbed {
                model_name: "minilm".to_string(),
                embedder_id: "minilm-384".to_string()
            }
        );
        assert_eq!(
            resolve_embedder_kind("MINILM-384", true).unwrap(),
            WorkerEmbedderKind::FastEmbed {
                model_name: "minilm".to_string(),
                embedder_id: "minilm-384".to_string()
            }
        );
        assert_eq!(
            resolve_embedder_kind("fastembed", true).unwrap(),
            WorkerEmbedderKind::FastEmbed {
                model_name: "minilm".to_string(),
                embedder_id: "minilm-384".to_string()
            }
        );
    }

    #[test]
    fn test_resolve_embedder_kind_registered_fastembed_models() {
        assert_eq!(
            resolve_embedder_kind("snowflake-arctic-s", true).unwrap(),
            WorkerEmbedderKind::FastEmbed {
                model_name: "snowflake-arctic-s".to_string(),
                embedder_id: "snowflake-arctic-s-384".to_string()
            }
        );
        assert_eq!(
            resolve_embedder_kind("nomic-embed-text-v1.5", true).unwrap(),
            WorkerEmbedderKind::FastEmbed {
                model_name: "nomic-embed".to_string(),
                embedder_id: "nomic-embed-768".to_string()
            }
        );
    }

    #[test]
    fn test_resolve_embedder_kind_rejects_unknown_semantic_model() {
        let err = resolve_embedder_kind("e5-large", true).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("unsupported semantic model"));
    }
}
