//! Background embedding worker for the daemon.
//!
//! Processes embedding jobs on a dedicated thread using sync primitives.
//! Adapted from xf's async worker to cass's sync daemon architecture.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use tracing::{debug, error, info, warn};

use crate::indexer::semantic::{
    EmbeddingInput, SemanticIndexer, expected_vector_space_revision, message_id_from_db,
    saturating_u32_from_i64, semantic_doc_id_for_input,
};
use crate::search::canonicalize::canonicalize_for_embedding;
use crate::search::fastembed_embedder::FastEmbedder;
use crate::search::semantic_manifest::TierKind;
use crate::search::vector_index::{VectorIndex, role_code_from_str, vector_index_path};
use crate::storage::sqlite::FrankenStorage;

const HASH_EMBEDDER_MODEL: &str = "hash";
const DEFAULT_SEMANTIC_MODEL: &str = "minilm";

/// How many documents to embed per progress/cancellation checkpoint. Matches
/// the healthy MiniLM batch size observed in cass#257 telemetry.
const EMBED_PROGRESS_CHUNK_SIZE: usize = 128;

/// How an embedding pass ended: normally, or via a user cancel (which must be
/// recorded as cancelled, not failed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmbeddingPassOutcome {
    Completed,
    Cancelled,
}

/// Configuration for a single embedding job.
#[derive(Debug, Clone)]
pub struct EmbeddingJobConfig {
    pub db_path: String,
    pub index_path: String,
    pub two_tier: bool,
    pub fast_model: Option<String>,
    pub quality_model: Option<String>,
}

impl EmbeddingJobConfig {
    fn fast_pass_model(&self) -> String {
        self.fast_model
            .clone()
            .unwrap_or_else(|| HASH_EMBEDDER_MODEL.to_string())
    }

    fn quality_pass_model(&self) -> String {
        self.quality_model
            .clone()
            .unwrap_or_else(|| DEFAULT_SEMANTIC_MODEL.to_string())
    }

    fn single_pass_model(&self) -> String {
        self.quality_model
            .clone()
            .or_else(|| self.fast_model.clone())
            .unwrap_or_else(|| HASH_EMBEDDER_MODEL.to_string())
    }
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

/// The (db_path, model) pass the worker thread is currently embedding, used
/// by the handle to decide whether a cancel targets the running job or only
/// needs database-level cleanup.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RunningEmbeddingPass {
    db_path: String,
    model: String,
}

/// Handle for sending messages to the background worker.
#[derive(Clone)]
pub struct EmbeddingWorkerHandle {
    sender: Sender<WorkerMessage>,
    /// Shared cancel flag — set directly from the handle so cancellation
    /// takes effect even while `process_job` is running on the worker thread.
    cancel_flag: Arc<AtomicBool>,
    /// The pass currently running on the worker thread, if any.
    running_pass: Arc<Mutex<Option<RunningEmbeddingPass>>>,
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
    /// Sets the cancel flag directly — but only when the worker is currently
    /// running a pass for that `db_path` (and `model_id`, when given) — so a
    /// cancel aimed at one data dir can never abort another client's job.
    /// Always sends a Cancel message for database-level cleanup.
    pub fn cancel(&self, db_path: String, model_id: Option<String>) -> Result<(), String> {
        let targets_running_job = self
            .running_pass
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
            .is_some_and(|running| {
                running.db_path == db_path
                    && model_id
                        .as_deref()
                        .is_none_or(|model| running.model == model)
            });
        if targets_running_job {
            self.cancel_flag.store(true, Ordering::SeqCst);
        }
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
    running_pass: Arc<Mutex<Option<RunningEmbeddingPass>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WorkerEmbedderKind {
    Hash,
    FastEmbed {
        model_name: String,
        embedder_id: String,
        dimension: usize,
    },
}

#[derive(Debug, Default)]
struct ExistingIndexState {
    path_exists: bool,
    readable: bool,
    compatible: bool,
    active_doc_counts: HashMap<String, usize>,
    record_count: usize,
    tombstone_count: usize,
    wal_record_count: usize,
}

impl ExistingIndexState {
    fn active_count(&self, doc_id: &str) -> usize {
        if !self.compatible {
            return 0;
        }
        self.active_doc_counts.get(doc_id).copied().unwrap_or(0)
    }

    fn exactly_matches(&self, current_doc_ids: &HashSet<String>) -> bool {
        self.path_exists
            && self.readable
            && self.compatible
            && self.tombstone_count == 0
            && self.wal_record_count == 0
            && self.record_count == current_doc_ids.len()
            && self.active_doc_counts.len() == current_doc_ids.len()
            && current_doc_ids
                .iter()
                .all(|doc_id| self.active_count(doc_id) == 1)
    }
}

fn resolve_embedder_kind(
    model_name: &str,
    use_semantic: bool,
) -> anyhow::Result<WorkerEmbedderKind> {
    if !use_semantic
        || model_name.eq_ignore_ascii_case(HASH_EMBEDDER_MODEL)
        || model_name.eq_ignore_ascii_case("fnv1a-384")
    {
        return Ok(WorkerEmbedderKind::Hash);
    }

    let normalized_name = match model_name.to_ascii_lowercase().as_str() {
        "fastembed" | "minilm" | "minilm-384" | "all-minilm-l6-v2" => DEFAULT_SEMANTIC_MODEL,
        _ => {
            anyhow::bail!(
                "unsupported semantic model '{model_name}' for daemon embedding worker; the pure-Rust native backend supports only minilm (all-MiniLM-L6-v2)"
            );
        }
    };

    let config = FastEmbedder::config_for(normalized_name).ok_or_else(|| {
        anyhow::anyhow!("missing FastEmbedder config for registered model '{normalized_name}'")
    })?;
    Ok(WorkerEmbedderKind::FastEmbed {
        model_name: normalized_name.to_string(),
        embedder_id: config.embedder_id,
        dimension: config.dimension,
    })
}

fn saturating_i64_from_usize(raw: usize) -> i64 {
    i64::try_from(raw).unwrap_or(i64::MAX)
}

impl EmbeddingWorker {
    /// Create a new worker and its handle.
    pub fn new() -> (Self, EmbeddingWorkerHandle) {
        let (sender, receiver) = std::sync::mpsc::channel();
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let running_pass = Arc::new(Mutex::new(None));
        let handle = EmbeddingWorkerHandle {
            sender,
            cancel_flag: Arc::clone(&cancel_flag),
            running_pass: Arc::clone(&running_pass),
        };
        let worker = Self {
            receiver,
            cancel_flag,
            running_pass,
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
        let total_docs = saturating_i64_from_usize(messages.len());

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

            if let Ok(mut guard) = self.running_pass.lock() {
                *guard = Some(RunningEmbeddingPass {
                    db_path: config.db_path.clone(),
                    model: model_name.clone(),
                });
            }
            let pass_result = self.generate_embeddings_and_save(
                &storage,
                &messages,
                model_name,
                *use_semantic,
                job_id,
                index_path,
                db_path,
            );
            if let Ok(mut guard) = self.running_pass.lock() {
                *guard = None;
            }

            match pass_result {
                Ok(EmbeddingPassOutcome::Completed) => {
                    storage.complete_embedding_job(job_id)?;
                    info!(model = model_name, "Embedding pass completed");
                }
                Ok(EmbeddingPassOutcome::Cancelled) => {
                    // A user cancel is not a failure — record it as cancelled
                    // so job status matches what actually happened.
                    let _ = storage.cancel_embedding_jobs(&config.db_path, Some(model_name));
                    info!(model = model_name, "Embedding pass cancelled");
                    return Ok(());
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
            let fast = config.fast_pass_model();
            passes.push((fast, false));

            // Quality semantic pass
            let quality = config.quality_pass_model();
            passes.push((quality, true));
        } else {
            // Single pass with best available
            let model = config.single_pass_model();
            let is_semantic = model != HASH_EMBEDDER_MODEL;
            passes.push((model, is_semantic));
        }

        passes
    }

    /// Generate embeddings for messages and save the vector index.
    #[allow(clippy::too_many_arguments)]
    fn generate_embeddings_and_save(
        &self,
        storage: &FrankenStorage,
        messages: &[crate::storage::sqlite::MessageForEmbedding],
        model_name: &str,
        use_semantic: bool,
        job_id: i64,
        index_path: &Path,
        db_path: &Path,
    ) -> anyhow::Result<EmbeddingPassOutcome> {
        let embedder_kind = resolve_embedder_kind(model_name, use_semantic)?;

        // Build the canonical identities before deciding what to embed. A
        // message-id-to-hash map is insufficient here: doc IDs also encode
        // metadata, duplicate live records are possible, and an edited row's
        // old identity must be removed rather than merely appended around.
        let mut canonical_inputs: Vec<(String, EmbeddingInput)> = Vec::new();
        let mut skipped_count = 0usize;
        let mut completed = 0i64;

        for msg in messages {
            if self.cancel_flag.load(Ordering::SeqCst) {
                return Ok(EmbeddingPassOutcome::Cancelled);
            }

            let canonical = canonicalize_for_embedding(&msg.content);
            if canonical.is_empty() {
                completed += 1;
                continue;
            }

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

            // Clamp to a stable range instead of silently wrapping/failing.
            let agent_id = saturating_u32_from_i64(msg.agent_id);
            let workspace_id = saturating_u32_from_i64(msg.workspace_id.unwrap_or(0));

            let input = EmbeddingInput {
                message_id,
                created_at_ms: msg.created_at.unwrap_or(0),
                agent_id,
                workspace_id,
                source_id: msg.source_id_hash,
                role,
                chunk_idx: 0,
                content: canonical,
            };
            let Some(doc_id) = semantic_doc_id_for_input(&input) else {
                completed += 1;
                continue;
            };
            canonical_inputs.push((doc_id, input));
        }

        let current_doc_ids = canonical_inputs
            .iter()
            .map(|(doc_id, _)| doc_id.clone())
            .collect::<HashSet<_>>();
        if current_doc_ids.len() != canonical_inputs.len() {
            anyhow::bail!("daemon embedding input contains duplicate canonical document IDs");
        }

        let existing_state = self.load_existing_index_state(index_path, &embedder_kind);
        if existing_state.exactly_matches(&current_doc_ids) {
            let final_completed = saturating_i64_from_usize(messages.len());
            let _ = storage.update_job_progress(job_id, final_completed);
            info!(
                model = model_name,
                skipped = canonical_inputs.len(),
                "Semantic index already exactly matches the canonical database"
            );
            return Ok(EmbeddingPassOutcome::Completed);
        }

        let inputs = canonical_inputs
            .into_iter()
            .filter_map(|(doc_id, input)| {
                if existing_state.active_count(&doc_id) == 1 {
                    skipped_count += 1;
                    completed += 1;
                    None
                } else {
                    Some(input)
                }
            })
            .collect::<Vec<_>>();

        // `completed` so far counts only documents that are genuinely done
        // (empty, invalid, or unchanged). Documents queued for embedding are
        // counted as their chunk finishes, so job progress reflects the
        // expensive embedding work instead of racing to ~100% during the
        // cheap scan phase.
        let _ = storage.update_job_progress(job_id, completed);

        if inputs.is_empty() && !existing_state.path_exists {
            let final_completed = saturating_i64_from_usize(messages.len());
            let _ = storage.update_job_progress(job_id, final_completed);
            info!(
                model = model_name,
                skipped = skipped_count,
                "No canonical documents and no semantic index to reconcile"
            );
            return Ok(EmbeddingPassOutcome::Completed);
        }

        info!(
            model = model_name,
            input_count = inputs.len(),
            skipped = skipped_count,
            "Embedding documents"
        );

        // Create the appropriate embedder/indexer
        let indexer = match &embedder_kind {
            WorkerEmbedderKind::Hash => SemanticIndexer::new(HASH_EMBEDDER_MODEL, None)?,
            WorkerEmbedderKind::FastEmbed { model_name, .. } => {
                SemanticIndexer::new(model_name, Some(index_path))?
            }
        };

        // Embed in bounded chunks so a long semantic pass reports progress
        // and honors cancellation between chunks instead of running as one
        // opaque, uncancellable block.
        let mut embedded = Vec::with_capacity(inputs.len());
        for chunk in inputs.chunks(EMBED_PROGRESS_CHUNK_SIZE) {
            if self.cancel_flag.load(Ordering::SeqCst) {
                return Ok(EmbeddingPassOutcome::Cancelled);
            }
            embedded.extend(indexer.embed_messages(chunk)?);
            completed += saturating_i64_from_usize(chunk.len());
            let _ = storage.update_job_progress(job_id, completed);
            debug!(job_id, completed, "Embedding progress");
        }

        // Update final progress
        let final_completed = saturating_i64_from_usize(messages.len());
        let _ = storage.update_job_progress(job_id, final_completed);

        // Existing artifacts are reconciled through a private candidate and
        // atomic publish. This removes stale identities, collapses duplicate
        // live records, clears tombstones/WAL state, and preserves exact
        // unchanged vectors without exposing a half-updated index.
        let save_path = vector_index_path(index_path, indexer.embedder_id());
        if existing_state.path_exists {
            let tier = match &embedder_kind {
                WorkerEmbedderKind::Hash => TierKind::Fast,
                WorkerEmbedderKind::FastEmbed { .. } => TierKind::Quality,
            };
            let db_fingerprint = crate::indexer::lexical_storage_fingerprint_for_db(db_path)?;
            let reconciled = indexer.reconcile_index_with_canonical_documents(
                embedded,
                index_path,
                tier,
                &db_fingerprint,
                &current_doc_ids,
            )?;
            info!(
                published = reconciled.record_count(),
                "Reconciled semantic index with canonical documents"
            );
        } else {
            let _index = indexer.build_and_save_index(embedded, index_path)?;
        }

        info!(
            model = model_name,
            path = %save_path.display(),
            count = inputs.len(),
            "Saved vector index"
        );

        Ok(EmbeddingPassOutcome::Completed)
    }

    /// Load exact active document identities from an existing vector index.
    fn load_existing_index_state(
        &self,
        index_path: &Path,
        embedder_kind: &WorkerEmbedderKind,
    ) -> ExistingIndexState {
        let embedder_id = match embedder_kind {
            WorkerEmbedderKind::Hash => "fnv1a-384",
            WorkerEmbedderKind::FastEmbed { embedder_id, .. } => embedder_id.as_str(),
        };
        let expected_dimension = match embedder_kind {
            WorkerEmbedderKind::Hash => crate::search::hash_embedder::DEFAULT_DIMENSION,
            WorkerEmbedderKind::FastEmbed { dimension, .. } => *dimension,
        };

        let fsvi_path = vector_index_path(index_path, embedder_id);

        if !fsvi_path.exists() {
            return ExistingIndexState::default();
        }

        match VectorIndex::open(&fsvi_path) {
            Ok(index) => {
                let compatible =
                    expected_vector_space_revision(embedder_id).is_some_and(|expected_revision| {
                        index.embedder_id() == embedder_id
                            && index.dimension() == expected_dimension
                            && index.embedder_revision() == expected_revision
                    });
                let mut active_doc_counts = HashMap::new();
                for idx in 0..index.record_count() {
                    if index.is_deleted(idx) {
                        continue;
                    }
                    let doc_id_str = match index.doc_id_at(idx) {
                        Ok(doc_id) => doc_id,
                        Err(_) => continue,
                    };
                    *active_doc_counts.entry(doc_id_str.to_owned()).or_insert(0) += 1;
                }
                debug!(
                    path = %fsvi_path.display(),
                    active = active_doc_counts.len(),
                    records = index.record_count(),
                    tombstones = index.tombstone_count(),
                    wal_records = index.wal_record_count(),
                    compatible,
                    "Loaded existing semantic identities for reconciliation"
                );
                ExistingIndexState {
                    path_exists: true,
                    readable: true,
                    compatible,
                    active_doc_counts,
                    record_count: index.record_count(),
                    tombstone_count: index.tombstone_count(),
                    wal_record_count: index.wal_record_count(),
                }
            }
            Err(e) => {
                warn!(
                    path = %fsvi_path.display(),
                    error = %e,
                    "Failed to load existing index for reconciliation"
                );
                ExistingIndexState {
                    path_exists: true,
                    ..ExistingIndexState::default()
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_pass_config(
        two_tier: bool,
        fast_model: Option<&str>,
        quality_model: Option<&str>,
    ) -> EmbeddingJobConfig {
        EmbeddingJobConfig {
            db_path: String::new(),
            index_path: String::new(),
            two_tier,
            fast_model: fast_model.map(str::to_string),
            quality_model: quality_model.map(str::to_string),
        }
    }

    fn fast_embed_kind(model_name: &str, embedder_id: &str) -> WorkerEmbedderKind {
        WorkerEmbedderKind::FastEmbed {
            model_name: model_name.to_string(),
            embedder_id: embedder_id.to_string(),
            dimension: 384,
        }
    }

    #[test]
    fn cancel_only_flags_matching_running_pass() {
        let (worker, handle) = EmbeddingWorker::new();
        if let Ok(mut guard) = worker.running_pass.lock() {
            *guard = Some(RunningEmbeddingPass {
                db_path: "/data/a.db".to_string(),
                model: "minilm".to_string(),
            });
        }

        // Different db_path: must not abort the running job.
        assert!(handle.cancel("/data/b.db".to_string(), None).is_ok());
        assert!(
            !worker.cancel_flag.load(Ordering::SeqCst),
            "cancel for another db_path must not flag the running job"
        );

        // Same db_path but different model: must not abort the running pass.
        assert!(
            handle
                .cancel("/data/a.db".to_string(), Some("hash".to_string()))
                .is_ok()
        );
        assert!(
            !worker.cancel_flag.load(Ordering::SeqCst),
            "cancel for another model must not flag the running pass"
        );

        // Matching target: flags the running job.
        assert!(
            handle
                .cancel("/data/a.db".to_string(), Some("minilm".to_string()))
                .is_ok()
        );
        assert!(
            worker.cancel_flag.load(Ordering::SeqCst),
            "cancel matching the running pass must set the flag"
        );
    }

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
        let config = build_pass_config(false, None, Some("minilm"));
        let passes = _worker.build_passes(&config);
        assert_eq!(passes.len(), 1);
        assert_eq!(passes[0].0, "minilm");
        assert!(passes[0].1); // semantic
    }

    #[test]
    fn test_build_passes_two_tier() {
        let (_worker, _handle) = EmbeddingWorker::new();
        let config = build_pass_config(true, Some("hash"), Some("minilm"));
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
        let config = build_pass_config(false, None, None);
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
    fn test_saturating_i64_from_usize_clamps_overflow() {
        assert_eq!(saturating_i64_from_usize(0), 0);
        assert_eq!(saturating_i64_from_usize(7), 7);
        assert_eq!(
            saturating_i64_from_usize(usize::MAX),
            i64::try_from(usize::MAX).unwrap_or(i64::MAX)
        );
    }

    #[test]
    fn daemon_embedding_reconciles_edited_and_removed_messages() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("agent_search.db");
        let index_path = temp.path().join("semantic");
        let storage = FrankenStorage::open(&db_path).unwrap();
        let (worker, _handle) = EmbeddingWorker::new();
        let job_id = storage
            .upsert_embedding_job(&db_path.to_string_lossy(), HASH_EMBEDDER_MODEL, 1)
            .unwrap();
        storage.start_embedding_job(job_id).unwrap();

        let first = crate::storage::sqlite::MessageForEmbedding {
            message_id: 41,
            created_at: Some(1_700_000_000_000),
            agent_id: 7,
            workspace_id: Some(9),
            source_id_hash: 11,
            role: "assistant".to_string(),
            content: "the original semantic content".to_string(),
        };
        assert_eq!(
            worker
                .generate_embeddings_and_save(
                    &storage,
                    std::slice::from_ref(&first),
                    HASH_EMBEDDER_MODEL,
                    false,
                    job_id,
                    &index_path,
                    &db_path,
                )
                .unwrap(),
            EmbeddingPassOutcome::Completed
        );

        let fsvi_path = vector_index_path(&index_path, "fnv1a-384");
        let initial = VectorIndex::open(&fsvi_path).unwrap();
        assert_eq!(initial.record_count(), 1);
        let original_doc_id = initial.doc_id_at(0).unwrap().to_owned();
        drop(initial);

        let edited = crate::storage::sqlite::MessageForEmbedding {
            content: "the corrected semantic content".to_string(),
            ..first
        };
        assert_eq!(
            worker
                .generate_embeddings_and_save(
                    &storage,
                    std::slice::from_ref(&edited),
                    HASH_EMBEDDER_MODEL,
                    false,
                    job_id,
                    &index_path,
                    &db_path,
                )
                .unwrap(),
            EmbeddingPassOutcome::Completed
        );

        let reconciled = VectorIndex::open(&fsvi_path).unwrap();
        assert_eq!(reconciled.record_count(), 1);
        assert_eq!(reconciled.tombstone_count(), 0);
        assert_eq!(reconciled.wal_record_count(), 0);
        let edited_doc_id = reconciled.doc_id_at(0).unwrap().to_owned();
        assert_ne!(edited_doc_id, original_doc_id);
        drop(reconciled);

        assert_eq!(
            worker
                .generate_embeddings_and_save(
                    &storage,
                    &[],
                    HASH_EMBEDDER_MODEL,
                    false,
                    job_id,
                    &index_path,
                    &db_path,
                )
                .unwrap(),
            EmbeddingPassOutcome::Completed
        );
        let emptied = VectorIndex::open(&fsvi_path).unwrap();
        assert_eq!(emptied.record_count(), 0);
        assert_eq!(emptied.tombstone_count(), 0);
        assert_eq!(emptied.wal_record_count(), 0);
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
    /// any configured name MUST resolve to `Hash` purely
    /// because the flag is false, regardless of name.
    #[test]
    fn test_resolve_embedder_kind_use_semantic_false_short_circuits_regardless_of_name() {
        for semantic_name in [
            "minilm",
            "minilm-384",
            "all-minilm-l6-v2",
            "fastembed",
            "legacy-unavailable-model",
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
            fast_embed_kind("minilm", "minilm-384")
        );
        assert_eq!(
            resolve_embedder_kind("MINILM-384", true).unwrap(),
            fast_embed_kind("minilm", "minilm-384")
        );
        assert_eq!(
            resolve_embedder_kind("fastembed", true).unwrap(),
            fast_embed_kind("minilm", "minilm-384")
        );
    }

    #[test]
    fn test_resolve_embedder_kind_rejects_unverified_native_topologies() -> anyhow::Result<()> {
        for model in ["snowflake-arctic-s", "nomic-embed-text-v1.5"] {
            let Err(error) = resolve_embedder_kind(model, true) else {
                anyhow::bail!("unverified model topology {model} was accepted");
            };
            let message = format!("{error:#}");
            assert!(message.contains("supports only minilm"), "{message}");
        }
        Ok(())
    }

    #[test]
    fn test_resolve_embedder_kind_rejects_unknown_semantic_model() {
        let err = resolve_embedder_kind("e5-large", true).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("unsupported semantic model"));
    }
}
