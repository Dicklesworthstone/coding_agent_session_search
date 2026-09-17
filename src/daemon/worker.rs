//! Background embedding worker for the daemon.
//!
//! Processes embedding jobs on a dedicated thread using sync primitives.
//! Adapted from xf's async worker to cass's sync daemon architecture.

mod embedding_source;
mod work_queue;

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tracing::{debug, error, info, warn};

use self::embedding_source::{EmbeddingMessageSource, SqliteEmbeddingSource};
use self::work_queue::{JobControl, Receiver, Sender};
use crate::indexer::semantic::{
    EmbeddingInput, SemanticIndexer, expected_vector_space_revision, message_id_from_db,
    saturating_u32_from_i64, semantic_doc_id_for_input,
};
use crate::search::canonicalize::embedding_passages;
use crate::search::fastembed_embedder::FastEmbedder;
use crate::search::semantic_manifest::TierKind;
use crate::search::vector_index::{VectorIndex, role_code_from_str, vector_index_path};
use crate::storage::sqlite::FrankenStorage;

const HASH_EMBEDDER_MODEL: &str = "hash";
const DEFAULT_SEMANTIC_MODEL: &str = "minilm";

/// Maximum passages retained for one embedding/progress checkpoint.
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

/// The (db_path, model) pass the worker thread is currently embedding.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RunningEmbeddingPass {
    db_path: String,
    model: String,
}

/// Clear the running identity on every exit, including archive-open errors.
struct RunningPassReset<'a>(&'a Mutex<Option<RunningEmbeddingPass>>);

impl Drop for RunningPassReset<'_> {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.0.lock() {
            *guard = None;
        }
    }
}

/// Handle for admitting bounded jobs and issuing job-scoped cancellation.
#[derive(Clone)]
pub struct EmbeddingWorkerHandle {
    sender: Sender,
    cancel_flag: Arc<AtomicBool>,
    shutdown_requested: Arc<AtomicBool>,
}

impl EmbeddingWorkerHandle {
    /// Admit a job without waiting for queue space. Identical pending requests
    /// coalesce; full queues return a retryable admission failure to the daemon.
    pub fn submit(&self, config: EmbeddingJobConfig) -> Result<(), String> {
        if self.shutdown_requested.load(Ordering::SeqCst) {
            return Err("embedding worker is shutting down".to_string());
        }
        self.sender.send(WorkerMessage::Submit(config))
    }

    /// Cancel matching passes in already-admitted jobs. The mailbox registers
    /// active ownership at dequeue, so cancellation cannot miss the interval
    /// before archive admission. Later submissions receive fresh tokens.
    pub fn cancel(&self, db_path: String, model_id: Option<String>) -> Result<(), String> {
        self.sender.send(WorkerMessage::Cancel { db_path, model_id })
    }

    /// Request cooperative shutdown immediately, not after queued jobs finish.
    /// An in-flight engine operation or embedding batch finishes before its
    /// next cancellation checkpoint; this does not forcibly kill the thread.
    pub fn shutdown(&self) -> Result<(), String> {
        self.shutdown_requested.store(true, Ordering::SeqCst);
        self.cancel_flag.store(true, Ordering::SeqCst);
        self.sender.send(WorkerMessage::Shutdown)
    }
}

/// Background embedding worker that processes jobs on a dedicated thread.
pub struct EmbeddingWorker {
    receiver: Receiver,
    cancel_flag: Arc<AtomicBool>,
    shutdown_requested: Arc<AtomicBool>,
    running_pass: Arc<Mutex<Option<RunningEmbeddingPass>>>,
    active_control: Mutex<Option<JobControl>>,
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

    let normalized_name = FastEmbedder::canonical_name(model_name).ok_or_else(|| {
        anyhow::anyhow!(
            "unsupported semantic model '{model_name}' for daemon embedding worker; the pure-Rust native backend supports minilm and the explicit multilingual-minilm profile"
        )
    })?;

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

/// Prepare at most one message's bounded passages, never the whole archive.
fn canonical_message_inputs(
    message: &crate::storage::sqlite::MessageForEmbedding,
) -> anyhow::Result<Vec<(String, EmbeddingInput)>> {
    let Some(message_id) = message_id_from_db(message.message_id) else {
        return Ok(Vec::new());
    };
    let mut inputs = Vec::new();
    for (ordinal, passage) in embedding_passages(&message.content).into_iter().enumerate() {
        let input = EmbeddingInput {
            message_id,
            created_at_ms: message.created_at.unwrap_or(0),
            agent_id: saturating_u32_from_i64(message.agent_id),
            workspace_id: saturating_u32_from_i64(message.workspace_id.unwrap_or(0)),
            source_id: message.source_id_hash,
            role: role_code_from_str(&message.role).unwrap_or(0),
            chunk_idx: u8::try_from(ordinal)?,
            content: passage.to_owned(),
        };
        if let Some(doc_id) = semantic_doc_id_for_input(&input) {
            inputs.push((doc_id, input));
        }
    }
    Ok(inputs)
}

impl EmbeddingWorker {
    /// Create a new worker and its handle.
    pub fn new() -> (Self, EmbeddingWorkerHandle) {
        let (sender, receiver) = work_queue::channel();
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let shutdown_requested = Arc::new(AtomicBool::new(false));
        let running_pass = Arc::new(Mutex::new(None));
        let handle = EmbeddingWorkerHandle {
            sender,
            cancel_flag: Arc::clone(&cancel_flag),
            shutdown_requested: Arc::clone(&shutdown_requested),
        };
        let worker = Self {
            receiver,
            cancel_flag,
            shutdown_requested,
            running_pass,
            active_control: Mutex::new(None),
        };
        (worker, handle)
    }

    fn is_stopping(&self) -> bool {
        self.cancel_flag.load(Ordering::SeqCst)
            || self.shutdown_requested.load(Ordering::SeqCst)
    }

    fn pass_was_cancelled(&self, model: &str) -> bool {
        self.active_control
            .lock()
            .map(|control| control.as_ref().is_some_and(|control| control.is_cancelled(model)))
            .unwrap_or(true)
    }

    fn all_passes_cancelled(&self) -> bool {
        self.active_control
            .lock()
            .map(|control| control.as_ref().is_some_and(JobControl::all_cancelled))
            .unwrap_or(true)
    }

    fn is_cancelled(&self) -> bool {
        if self.is_stopping() {
            return true;
        }
        match self.running_pass.lock() {
            Ok(pass) => match pass.as_ref() {
                Some(pass) => self.pass_was_cancelled(&pass.model),
                None => self.all_passes_cancelled(),
            },
            Err(_) => true,
        }
    }

    fn set_running_pass(&self, db_path: &str, model: &str) -> anyhow::Result<()> {
        let mut guard = self
            .running_pass
            .lock()
            .map_err(|_| anyhow::anyhow!("embedding worker state lock poisoned"))?;
        *guard = Some(RunningEmbeddingPass {
            db_path: db_path.to_string(),
            model: model.to_string(),
        });
        Ok(())
    }

    /// Run the worker loop (blocking). Call from a spawned thread.
    pub fn run(self) {
        info!("Embedding worker started");
        while let Ok((msg, permit)) = self.receiver.recv() {
            if self.shutdown_requested.load(Ordering::SeqCst) {
                break;
            }
            match msg {
                WorkerMessage::Submit(config) => {
                    let Some(permit) = permit.as_ref() else {
                        error!("embedding submission has no job ownership permit");
                        break;
                    };
                    match self.active_control.lock() {
                        Ok(mut control) => *control = Some(permit.control()),
                        Err(_) => {
                            error!("embedding worker state lock poisoned");
                            break;
                        }
                    }
                    self.cancel_flag.store(false, Ordering::SeqCst);
                    info!(db_path = %config.db_path, two_tier = config.two_tier, "Processing embedding job");
                    if let Err(e) = self.process_job(&config) {
                        error!(db_path = %config.db_path, error = %e, "Embedding job failed");
                    }
                    match self.active_control.lock() {
                        Ok(mut control) => *control = None,
                        Err(_) => break,
                    }
                }
                WorkerMessage::Cancel { db_path, model_id } => {
                    // The queue already cancelled in-memory ownership. Process
                    // durable cleanup before any newer submission can start.
                    info!(%db_path, ?model_id, "Processing embedding cancellation cleanup");
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

    /// Cancel persisted jobs without creating an archive for an absent target.
    fn cancel_in_db(db_path: &str, model_id: Option<&str>) -> anyhow::Result<()> {
        match std::fs::metadata(db_path) {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => anyhow::bail!("embedding cancellation target is not a regular file"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        }
        let storage = FrankenStorage::open(Path::new(db_path))?;
        storage.cancel_embedding_jobs(db_path, model_id)?;
        Ok(())
    }

    /// Process a single embedding job. A model-specific cancellation skips only
    /// that pass, including a future tier of a currently active two-tier job.
    fn process_job(&self, config: &EmbeddingJobConfig) -> anyhow::Result<()> {
        if self.is_stopping() || self.all_passes_cancelled() {
            return Ok(());
        }
        let db_path = Path::new(&config.db_path);
        let index_path = Path::new(&config.index_path);
        let passes = self.build_passes(config);
        let Some((first_model, _)) = passes
            .iter()
            .find(|(model, _)| !self.pass_was_cancelled(model))
        else {
            return Ok(());
        };
        self.set_running_pass(&config.db_path, first_model)?;
        let _running_reset = RunningPassReset(&self.running_pass);
        if self.is_stopping() || self.all_passes_cancelled() {
            return Ok(());
        }
        let storage = FrankenStorage::open(db_path)?;
        if self.is_stopping() || self.all_passes_cancelled() {
            return Ok(());
        }
        let messages = SqliteEmbeddingSource::open(db_path)?;
        let total_docs = saturating_i64_from_usize(messages.total_docs());
        info!(
            db_path = %config.db_path,
            total_docs,
            two_tier = config.two_tier,
            "Found messages to embed"
        );

        for (model_name, use_semantic) in &passes {
            if self.is_stopping() {
                return Ok(());
            }
            self.set_running_pass(&config.db_path, model_name)?;
            if self.pass_was_cancelled(model_name) {
                info!(model = model_name, "Skipping cancelled embedding pass");
                continue;
            }
            let job_id = storage.upsert_embedding_job(&config.db_path, model_name, total_docs)?;
            storage.start_embedding_job(job_id)?;
            let pass_result = self.generate_embeddings_from_source(
                &storage,
                &messages,
                model_name,
                *use_semantic,
                job_id,
                index_path,
                db_path,
            );
            match pass_result {
                Ok(EmbeddingPassOutcome::Completed) => {
                    storage.complete_embedding_job(job_id)?;
                    info!(model = model_name, "Embedding pass completed");
                }
                Ok(EmbeddingPassOutcome::Cancelled) => {
                    storage.cancel_embedding_jobs(&config.db_path, Some(model_name))?;
                    info!(model = model_name, "Embedding pass cancelled");
                    // A different tier remains eligible unless the whole job
                    // was cancelled or shutdown was requested.
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

    /// Keep the existing reconciliation regressions on the production path.
    #[cfg(test)]
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
        self.generate_embeddings_from_source(
            storage,
            &embedding_source::SliceEmbeddingSource(messages),
            model_name,
            use_semantic,
            job_id,
            index_path,
            db_path,
        )
    }

    /// Plan identities without retaining bodies, then replay the same source
    /// into bounded passage batches. Generated vectors and reconciliation
    /// identities remain corpus-sized; this is not a total-memory bound.
    #[allow(clippy::too_many_arguments)]
    fn generate_embeddings_from_source(
        &self,
        storage: &FrankenStorage,
        messages: &dyn EmbeddingMessageSource,
        model_name: &str,
        use_semantic: bool,
        job_id: i64,
        index_path: &Path,
        db_path: &Path,
    ) -> anyhow::Result<EmbeddingPassOutcome> {
        let cancelled = || self.is_cancelled();
        if cancelled() {
            return Ok(EmbeddingPassOutcome::Cancelled);
        }
        let embedder_kind = resolve_embedder_kind(model_name, use_semantic)?;
        let existing_state = self.load_existing_index_state(index_path, &embedder_kind);
        let mut current_doc_ids = HashSet::new();
        let mut pending_passages: HashMap<u64, usize> = HashMap::new();
        let mut skipped_count = 0usize;
        let mut input_count = 0usize;
        let mut completed = 0i64;

        let planned = messages.visit(&cancelled, &mut |message| {
            if message_id_from_db(message.message_id).is_none() {
                warn!(
                    raw_message_id = message.message_id,
                    "Skipping message with out-of-range id during embedding"
                );
            }
            let mut pending = 0usize;
            for (doc_id, input) in canonical_message_inputs(message)? {
                if !current_doc_ids.insert(doc_id.clone()) {
                    anyhow::bail!("daemon embedding input contains duplicate canonical document IDs");
                }
                if existing_state.active_count(&doc_id) == 1 {
                    skipped_count += 1;
                } else {
                    pending += 1;
                    input_count += 1;
                    *pending_passages.entry(input.message_id).or_default() += 1;
                }
            }
            if pending == 0 {
                completed = completed.saturating_add(1);
            }
            Ok(())
        })?;
        if !planned || cancelled() {
            return Ok(EmbeddingPassOutcome::Cancelled);
        }
        if existing_state.exactly_matches(&current_doc_ids) {
            storage.update_job_progress(job_id, saturating_i64_from_usize(messages.total_docs()))?;
            info!(
                model = model_name,
                skipped = current_doc_ids.len(),
                "Semantic index already exactly matches the canonical database"
            );
            return Ok(EmbeddingPassOutcome::Completed);
        }
        storage.update_job_progress(job_id, completed)?;
        if input_count == 0 && !existing_state.path_exists {
            info!(
                model = model_name,
                skipped = skipped_count,
                "No canonical documents and no semantic index to reconcile"
            );
            return Ok(EmbeddingPassOutcome::Completed);
        }
        info!(model = model_name, input_count, skipped = skipped_count, "Embedding documents");

        let indexer = match &embedder_kind {
            WorkerEmbedderKind::Hash => SemanticIndexer::new(HASH_EMBEDDER_MODEL, None)?,
            WorkerEmbedderKind::FastEmbed { model_name, .. } => {
                SemanticIndexer::new(model_name, Some(index_path))?
            }
        };
        let mut embedded = Vec::new();
        {
            let mut embed_chunk = |chunk: &[EmbeddingInput]| -> anyhow::Result<()> {
                let batch = indexer.embed_messages(chunk)?;
                if batch.len() != chunk.len() {
                    anyhow::bail!("daemon embedding batch returned an incomplete vector set");
                }
                embedded.extend(batch);
                for input in chunk {
                    let pending = pending_passages.get_mut(&input.message_id).ok_or_else(|| {
                        anyhow::anyhow!("embedded passage has no canonical message progress entry")
                    })?;
                    *pending = pending.checked_sub(1).ok_or_else(|| {
                        anyhow::anyhow!("daemon embedding source repeated a planned passage")
                    })?;
                    if *pending == 0 {
                        completed = completed.saturating_add(1);
                    }
                }
                storage.update_job_progress(job_id, completed)?;
                debug!(job_id, completed, "Embedding progress");
                Ok(())
            };
            let mut inputs = Vec::with_capacity(EMBED_PROGRESS_CHUNK_SIZE);
            let scanned = messages.visit(&cancelled, &mut |message| {
                for (doc_id, input) in canonical_message_inputs(message)? {
                    if !current_doc_ids.contains(&doc_id) {
                        anyhow::bail!("daemon embedding source changed after identity planning");
                    }
                    if existing_state.active_count(&doc_id) != 1 {
                        inputs.push(input);
                        if inputs.len() == EMBED_PROGRESS_CHUNK_SIZE {
                            embed_chunk(&inputs)?;
                            inputs.clear();
                        }
                    }
                }
                Ok(())
            })?;
            if !scanned || cancelled() {
                return Ok(EmbeddingPassOutcome::Cancelled);
            }
            if !inputs.is_empty() {
                embed_chunk(&inputs)?;
            }
        }
        if cancelled() {
            return Ok(EmbeddingPassOutcome::Cancelled);
        }
        if embedded.len() != input_count || pending_passages.values().any(|count| *count != 0) {
            anyhow::bail!("daemon embedding source did not yield every planned passage");
        }

        // Preserve the existing private-candidate/atomic-publication path:
        // stale identities disappear and unchanged vectors retain exact bits.
        let save_path = vector_index_path(index_path, indexer.embedder_id());
        if existing_state.path_exists {
            let tier = match &embedder_kind {
                WorkerEmbedderKind::Hash => TierKind::Fast,
                WorkerEmbedderKind::FastEmbed { .. } => TierKind::Quality,
            };
            let db_fingerprint = crate::indexer::lexical_storage_fingerprint_for_db(db_path)?;
            if cancelled() {
                return Ok(EmbeddingPassOutcome::Cancelled);
            }
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
        storage.update_job_progress(job_id, saturating_i64_from_usize(messages.total_docs()))?;
        info!(
            model = model_name,
            path = %save_path.display(),
            count = input_count,
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
        let mut config = build_pass_config(true, Some("hash"), Some("minilm"));
        config.db_path = "/data/a.db".into();
        config.index_path = "/data/index".into();
        handle.submit(config).unwrap();
        let (_, permit) = worker.receiver.recv().unwrap();
        *worker.active_control.lock().unwrap() = Some(permit.as_ref().unwrap().control());
        worker.set_running_pass("/data/a.db", "minilm").unwrap();

        assert!(handle.cancel("/data/b.db".to_string(), None).is_ok());
        assert!(!worker.is_cancelled(), "another archive must not cancel the running job");
        assert!(handle.cancel("/data/a.db".to_string(), Some("hash".to_string())).is_ok());
        assert!(!worker.is_cancelled(), "another model must not cancel the running pass");
        assert!(handle.cancel("/data/a.db".to_string(), Some("minilm".to_string())).is_ok());
        assert!(worker.is_cancelled(), "the matching model must cancel the running pass");
        drop(permit);
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
    fn gh470_daemon_passages_reconcile_tail_edits_shrink_and_unchanged() -> anyhow::Result<()> {
        use crate::search::canonicalize::{canonicalize_for_embedding, content_hash};
        use crate::search::embedder::Embedder;
        use crate::search::hash_embedder::HashEmbedder;
        use crate::search::vector_index::{SemanticDocId, parse_semantic_doc_id};

        let temp = tempfile::tempdir()?;
        let db_path = temp.path().join("archive.db");
        let index_path = temp.path().join("semantic");
        let storage = FrankenStorage::open(&db_path)?;
        let (worker, _) = EmbeddingWorker::new();
        let job_id = storage.upsert_embedding_job(&db_path.to_string_lossy(), "hash", 1)?;
        storage.start_embedding_job(job_id)?;
        let mut message = crate::storage::sqlite::MessageForEmbedding {
            message_id: 41,
            created_at: Some(1_700_000_000_000),
            agent_id: 7,
            workspace_id: Some(9),
            source_id_hash: 11,
            role: "assistant".into(),
            content: format!(
                "{}old tail",
                "Review the Unicode résumé and preserve each diagnostic source location. "
                    .repeat(100)
            ),
        };
        let run = |message: &crate::storage::sqlite::MessageForEmbedding, path: &Path| {
            worker.generate_embeddings_and_save(
                &storage,
                std::slice::from_ref(message),
                "hash",
                false,
                job_id,
                path,
                &db_path,
            )
        };
        let snapshot = |path: &Path| -> anyhow::Result<Vec<(SemanticDocId, Vec<u32>)>> {
            let index = VectorIndex::open(&vector_index_path(path, "fnv1a-384"))?;
            assert_eq!(index.wal_record_count(), 0);
            assert_eq!(index.tombstone_count(), 0);
            let mut records = Vec::new();
            for ordinal in 0..index.record_count() {
                let id = parse_semantic_doc_id(index.doc_id_at(ordinal)?)
                    .ok_or_else(|| anyhow::anyhow!("invalid passage identity"))?;
                assert_eq!((id.message_id, id.agent_id, id.workspace_id), (41, 7, 9));
                assert_eq!(
                    (id.source_id, id.role, id.created_at_ms),
                    (11, 1, 1_700_000_000_000)
                );
                let vector = index.vector_at_f32(ordinal)?;
                assert!(vector.iter().all(|value| value.is_finite()));
                records.push((id, vector.into_iter().map(f32::to_bits).collect()));
            }
            records.sort_by_key(|(id, _)| id.chunk_idx);
            Ok(records)
        };

        assert_eq!(run(&message, &index_path)?, EmbeddingPassOutcome::Completed);
        let original = snapshot(&index_path)?;
        assert_eq!(original.len(), 8);
        assert_eq!(
            original
                .iter()
                .map(|(id, _)| id.chunk_idx)
                .collect::<Vec<_>>(),
            (0..8).collect::<Vec<_>>()
        );
        let expected_head: String = message.content.chars().take(510).collect();
        assert_eq!(
            original[0].0.content_hash,
            Some(content_hash(&canonicalize_for_embedding(&expected_head)))
        );
        let before = std::fs::read(vector_index_path(&index_path, "fnv1a-384"))?;
        assert_eq!(run(&message, &index_path)?, EmbeddingPassOutcome::Completed);
        assert_eq!(
            std::fs::read(vector_index_path(&index_path, "fnv1a-384"))?,
            before
        );
        assert_eq!(snapshot(&index_path)?, original);

        // A real old producer embedded the canonical prefix once. Its header
        // and actual hash vector must be rebuilt, never relabeled as passages.
        let legacy_path = temp.path().join("legacy");
        let legacy_fsvi = vector_index_path(&legacy_path, "fnv1a-384");
        std::fs::create_dir_all(
            legacy_fsvi
                .parent()
                .ok_or_else(|| anyhow::anyhow!("missing vector parent"))?,
        )?;
        let prefix = canonicalize_for_embedding(&message.content);
        let legacy_id = SemanticDocId {
            content_hash: Some(content_hash(&prefix)),
            ..original[0].0
        };
        let legacy_vector = HashEmbedder::default().embed_sync(&prefix)?;
        let mut legacy = VectorIndex::create_with_revision(
            &legacy_fsvi,
            "fnv1a-384",
            "hash-fnv1a-modular-v1",
            384,
            frankensearch::index::Quantization::F16,
        )?;
        legacy.write_record(&legacy_id.to_doc_id_string(), &legacy_vector)?;
        legacy.finish()?;
        let source_before = message.content.clone();
        assert_eq!(
            run(&message, &legacy_path)?,
            EmbeddingPassOutcome::Completed
        );
        assert_eq!(message.content, source_before);
        assert_eq!(snapshot(&legacy_path)?, original);
        assert_eq!(
            VectorIndex::open(&legacy_fsvi)?.embedder_revision(),
            expected_vector_space_revision("fnv1a-384")
                .ok_or_else(|| anyhow::anyhow!("missing hash revision"))?
        );

        let old_tail = message.content.len() - "old tail".len();
        message.content.replace_range(old_tail.., "new tail");
        assert_eq!(run(&message, &index_path)?, EmbeddingPassOutcome::Completed);
        let edited = snapshot(&index_path)?;
        assert_eq!(edited.len(), 8);
        assert_eq!(
            &edited[..7],
            &original[..7],
            "unchanged passages retain exact metadata and vector bits"
        );
        assert_ne!(edited[7].0.content_hash, original[7].0.content_hash);
        assert_ne!(
            edited[7].1, original[7].1,
            "the changed tail must be embedded"
        );
        let fresh = temp.path().join("fresh");
        assert_eq!(run(&message, &fresh)?, EmbeddingPassOutcome::Completed);
        assert_eq!(
            snapshot(&fresh)?,
            edited,
            "reconciliation must equal a complete fresh embedding"
        );

        message.content = "short replacement retains the canonical message identity".into();
        assert_eq!(run(&message, &index_path)?, EmbeddingPassOutcome::Completed);
        let shrunk = snapshot(&index_path)?;
        assert_eq!(
            shrunk.len(),
            1,
            "obsolete long-message passages must be removed"
        );
        assert_eq!(shrunk[0].0.chunk_idx, 0);
        assert_eq!(
            shrunk[0].0.content_hash,
            Some(content_hash(&message.content))
        );
        let jobs = storage.get_embedding_jobs(&db_path.to_string_lossy())?;
        assert_eq!(jobs.len(), 1);
        assert_eq!((jobs[0].completed_docs, jobs[0].total_docs), (1, 1));
        Ok(())
    }

    #[test]
    fn gh470_daemon_progress_waits_for_the_last_passage_of_each_message() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let db_path = temp.path().join("archive.db");
        let storage = FrankenStorage::open(&db_path)?;
        let index_path = temp.path().join("semantic");
        let (worker, _) = EmbeddingWorker::new();
        let make_message =
            |message_id, content: String| crate::storage::sqlite::MessageForEmbedding {
                message_id,
                created_at: Some(1_700_000_000_000),
                agent_id: 1,
                workspace_id: None,
                source_id_hash: 2,
                role: "user".into(),
                content,
            };
        let mut messages = vec![make_message(1, "short leading message".into())];
        for id in 2..=17 {
            messages.push(make_message(
                id,
                format!("message {id}: Unicode café source evidence. ").repeat(150),
            ));
        }
        messages.push(make_message(18, " \n\t ".into()));
        messages.push(make_message(-1, "invalid canonical identity".into()));
        let job_id = storage.upsert_embedding_job(&db_path.to_string_lossy(), "hash", 19)?;
        storage.start_embedding_job(job_id)?;
        let trace_path = temp.path().join("worker-progress.jsonl");
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(Mutex::new(std::fs::File::create(&trace_path)?))
            .finish();
        let result = tracing::subscriber::with_default(subscriber, || {
            worker.generate_embeddings_and_save(
                &storage,
                &messages,
                "hash",
                false,
                job_id,
                &index_path,
                &db_path,
            )
        })?;
        assert_eq!(result, EmbeddingPassOutcome::Completed);
        let events = std::fs::read_to_string(&trace_path)?;
        let completed = events
            .lines()
            .map(serde_json::from_str::<serde_json::Value>)
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|event| event["fields"]["message"] == "Embedding progress")
            .map(|event| {
                event["fields"]["completed"]
                    .as_i64()
                    .ok_or_else(|| anyhow::anyhow!("missing completed count: {event}"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        // At 128 inputs, one short and 15 long messages are complete; the last
        // long message still has one pending passage. Empty/invalid rows add 2.
        assert_eq!(completed, [18, 19], "actual worker trace: {events}");
        let index = VectorIndex::open(&vector_index_path(&index_path, "fnv1a-384"))?;
        assert_eq!(index.record_count(), 129);
        drop(index);
        let jobs = storage.get_embedding_jobs(&db_path.to_string_lossy())?;
        assert_eq!((jobs[0].completed_docs, jobs[0].total_docs), (19, 19));
        assert_eq!(
            worker.generate_embeddings_and_save(
                &storage,
                &messages,
                "hash",
                false,
                job_id,
                &index_path,
                &db_path,
            )?,
            EmbeddingPassOutcome::Completed
        );
        assert_eq!(
            storage.get_embedding_jobs(&db_path.to_string_lossy())?[0].completed_docs,
            19
        );
        Ok(())
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
        assert_eq!(
            resolve_embedder_kind("multilingual-minilm", true).unwrap(),
            fast_embed_kind("multilingual-minilm", "multilingual-minilm-384")
        );
        assert_eq!(
            resolve_embedder_kind("paraphrase-multilingual-minilm-l12-v2", true).unwrap(),
            fast_embed_kind("multilingual-minilm", "multilingual-minilm-384")
        );
    }

    #[test]
    fn test_resolve_embedder_kind_rejects_unverified_native_topologies() -> anyhow::Result<()> {
        for model in ["snowflake-arctic-s", "nomic-embed-text-v1.5"] {
            let Err(error) = resolve_embedder_kind(model, true) else {
                anyhow::bail!("unverified model topology {model} was accepted");
            };
            let message = format!("{error:#}");
            assert!(message.contains("supports minilm"), "{message}");
            assert!(message.contains("multilingual-minilm"), "{message}");
        }
        Ok(())
    }

    #[test]
    fn test_resolve_embedder_kind_rejects_unknown_semantic_model() {
        let err = resolve_embedder_kind("e5-large", true).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("unsupported semantic model"));
    }

    #[test]
    fn shutdown_is_permanent_and_preempts_queued_submissions() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let db_path = temp.path().join("must-not-be-created.db");
        let index_path = temp.path().join("must-not-be-created-index");
        let config = EmbeddingJobConfig {
            db_path: db_path.to_string_lossy().into_owned(),
            index_path: index_path.to_string_lossy().into_owned(),
            two_tier: false,
            fast_model: Some("hash".into()),
            quality_model: None,
        };
        let (worker, handle) = EmbeddingWorker::new();
        handle.submit(config.clone()).map_err(anyhow::Error::msg)?;
        handle.shutdown().map_err(anyhow::Error::msg)?;
        assert!(worker.is_cancelled());
        // Model the job-start race: its reset must not erase shutdown intent.
        worker.cancel_flag.store(false, Ordering::SeqCst);
        assert!(worker.is_cancelled());
        assert!(handle.clone().submit(config).is_err());
        worker.run();
        assert!(!db_path.exists());
        assert!(!index_path.exists());
        Ok(())
    }

    #[test]
    fn archive_open_failure_clears_running_pass_identity() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let config = EmbeddingJobConfig {
            db_path: temp.path().to_string_lossy().into_owned(),
            index_path: temp.path().join("index").to_string_lossy().into_owned(),
            two_tier: false,
            fast_model: Some("hash".into()),
            quality_model: None,
        };
        let (worker, _) = EmbeddingWorker::new();
        assert!(worker.process_job(&config).is_err());
        assert!(worker.running_pass.lock().unwrap().is_none());
        Ok(())
    }

    #[test]
    fn cancellation_after_embedding_last_batch_preserves_published_index() -> anyhow::Result<()> {
        use std::cell::Cell;
        use crate::storage::sqlite::MessageForEmbedding;

        struct CancelAfterReplay {
            rows: Vec<MessageForEmbedding>,
            visits: Cell<usize>,
            cancel: Arc<AtomicBool>,
        }
        impl EmbeddingMessageSource for CancelAfterReplay {
            fn total_docs(&self) -> usize { self.rows.len() }
            fn visit(
                &self,
                cancelled: &dyn Fn() -> bool,
                visitor: &mut dyn FnMut(&MessageForEmbedding) -> anyhow::Result<()>,
            ) -> anyhow::Result<bool> {
                self.visits.set(self.visits.get() + 1);
                for row in &self.rows {
                    if cancelled() { return Ok(false); }
                    visitor(row)?;
                }
                if self.visits.get() == 2 {
                    self.cancel.store(true, Ordering::SeqCst);
                }
                Ok(!cancelled())
            }
        }
        let temp = tempfile::tempdir()?;
        let db_path = temp.path().join("archive.db");
        let index_path = temp.path().join("semantic");
        let storage = FrankenStorage::open(&db_path)?;
        let (worker, _) = EmbeddingWorker::new();
        let make_row = |id, content: String| MessageForEmbedding {
            message_id: id, created_at: Some(1700000000000), agent_id: 1,
            workspace_id: None, source_id_hash: 2, role: "user".into(), content,
        };
        let job_id = storage.upsert_embedding_job(&db_path.to_string_lossy(), "hash", 128)?;
        storage.start_embedding_job(job_id)?;
        let original = make_row(1, "original published content".into());
        assert_eq!(worker.generate_embeddings_and_save(
            &storage, std::slice::from_ref(&original), "hash", false, job_id,
            &index_path, &db_path,
        )?, EmbeddingPassOutcome::Completed);
        let path = vector_index_path(&index_path, "fnv1a-384");
        let before = std::fs::read(&path)?;
        let source = CancelAfterReplay {
            rows: (1..=128).map(|id| make_row(id, format!("replacement content {id}"))).collect(),
            visits: Cell::new(0),
            cancel: Arc::clone(&worker.cancel_flag),
        };
        assert_eq!(worker.generate_embeddings_from_source(
            &storage, &source, "hash", false, job_id, &index_path, &db_path,
        )?, EmbeddingPassOutcome::Cancelled);
        assert_eq!(source.visits.get(), 2);
        assert_eq!(std::fs::read(&path)?, before);
        Ok(())
    }

    #[test]
    fn queued_cancellation_does_not_create_a_database_or_index() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let config = EmbeddingJobConfig {
            db_path: temp.path().join("absent.db").to_string_lossy().into_owned(),
            index_path: temp.path().join("absent-index").to_string_lossy().into_owned(),
            two_tier: true,
            fast_model: Some("hash".into()),
            quality_model: Some("minilm".into()),
        };
        let (worker, handle) = EmbeddingWorker::new();
        handle.submit(config.clone()).map_err(anyhow::Error::msg)?;
        handle.cancel(config.db_path.clone(), None).map_err(anyhow::Error::msg)?;
        drop(handle);
        worker.run();
        assert!(!Path::new(&config.db_path).exists());
        assert!(!Path::new(&config.index_path).exists());
        assert_eq!(std::fs::read_dir(temp.path())?.count(), 0);
        Ok(())
    }

    #[test]
    fn queued_tier_cancellation_still_executes_the_other_empty_archive_pass() -> anyhow::Result<()> {
        for (cancelled_model, remaining_model) in [("hash", "minilm"), ("minilm", "hash")] {
            let temp = tempfile::tempdir()?;
            let db_path = temp.path().join("archive.db");
            let storage = FrankenStorage::open(&db_path)?;
            let config = EmbeddingJobConfig {
                db_path: db_path.to_string_lossy().into_owned(),
                index_path: temp.path().join("index").to_string_lossy().into_owned(),
                two_tier: true,
                fast_model: Some("hash".into()),
                quality_model: Some("minilm".into()),
            };
            let (worker, handle) = EmbeddingWorker::new();
            handle.submit(config.clone()).map_err(anyhow::Error::msg)?;
            handle.cancel(config.db_path.clone(), Some(cancelled_model.into())).map_err(anyhow::Error::msg)?;
            drop(handle);
            worker.run();
            let jobs = storage.get_embedding_jobs(&config.db_path)?;
            assert_eq!(jobs.len(), 1, "only the uncancelled tier should start");
            assert_eq!(jobs[0].model_id, remaining_model);
            assert_eq!(jobs[0].status, "completed");
            assert_eq!(jobs[0].total_docs, 0);
        }
        Ok(())
    }
}
