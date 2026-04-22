use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use frankensearch::index::{
    HNSW_DEFAULT_EF_CONSTRUCTION as FS_HNSW_DEFAULT_EF_CONSTRUCTION,
    HNSW_DEFAULT_M as FS_HNSW_DEFAULT_M, HnswConfig as FsHnswConfig, HnswIndex as FsHnswIndex,
    Quantization as FsQuantization, VectorIndex as FsVectorIndex,
    VectorIndexWriter as FsVectorIndexWriter,
};
use frankensqlite::compat::{ConnectionExt, ParamValue, RowExt};
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use rayon::prelude::*;

use crate::search::canonicalize::{canonicalize_for_embedding, content_hash};
use crate::search::embedder::Embedder;
use crate::search::fastembed_embedder::FastEmbedder;
use crate::search::hash_embedder::HashEmbedder;
use crate::search::policy::{CHUNKING_STRATEGY_VERSION, SEMANTIC_SCHEMA_VERSION};
use crate::search::semantic_manifest::{
    ArtifactRecord, BuildCheckpoint, SemanticManifest, TierKind,
};
use crate::search::vector_index::{
    ROLE_USER, SemanticDocId, VECTOR_INDEX_DIR, role_code_from_str, vector_index_path,
};
use crate::storage::sqlite::FrankenStorage;

/// Default embedder batch size. 128 is a sweet spot for ONNX MiniLM models on
/// modern CPUs: big enough to amortize dispatch overhead and keep the tensor
/// kernels saturated, small enough that one batch fits comfortably in L2 and
/// memory reservation stays bounded for large corpora.
const DEFAULT_SEMANTIC_BATCH_SIZE: usize = 128;

fn resolved_default_batch_size() -> usize {
    dotenvy::var("CASS_SEMANTIC_BATCH_SIZE")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_SEMANTIC_BATCH_SIZE)
}

/// Opt in to the rayon-parallel canonicalize+hash prep step. **Default: OFF.**
///
/// The parallel path is kept because canonicalize+hash CAN dominate the
/// embedding wall-clock on pathological inputs (very long messages, costly
/// Unicode normalization). But criterion baselines captured under
/// `tests/artifacts/perf/2026-04-21-profile-run/baselines.md` showed a
/// 1.2×–2.3× **regression** on the hash embedder across every batch size
/// tested (2 000 messages, mixed markdown/code/unicode): rayon's per-task
/// scheduling overhead is larger than the per-message canonicalize+hash cost
/// when the embedder itself is cheap. For the production ONNX (MiniLM)
/// embedder, per-batch inference already dwarfs prep, so parallel prep never
/// buys meaningful wall-clock — the prep step is ≤ 1% of total embed time.
///
/// Set `CASS_SEMANTIC_PREP_PARALLEL=1` / `true` / `yes` / `on` to opt in.
fn parallel_prep_enabled() -> bool {
    dotenvy::var("CASS_SEMANTIC_PREP_PARALLEL")
        .ok()
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            matches!(v.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(false)
}

#[derive(Debug, Clone)]
pub struct EmbeddingInput {
    pub message_id: u64,
    pub created_at_ms: i64,
    pub agent_id: u32,
    pub workspace_id: u32,
    pub source_id: u32,
    pub role: u8,
    pub chunk_idx: u8,
    pub content: String,
}

impl EmbeddingInput {
    pub fn new(message_id: u64, content: impl Into<String>) -> Self {
        Self {
            message_id,
            created_at_ms: 0,
            agent_id: 0,
            workspace_id: 0,
            source_id: 0,
            role: ROLE_USER,
            chunk_idx: 0,
            content: content.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct EmbeddedMessage {
    pub message_id: u64,
    pub created_at_ms: i64,
    pub agent_id: u32,
    pub workspace_id: u32,
    pub source_id: u32,
    pub role: u8,
    pub chunk_idx: u8,
    pub content_hash: [u8; 32],
    pub embedding: Vec<f32>,
}

#[derive(Debug, Clone)]
pub struct SemanticBackfillBatchPlan {
    pub tier: TierKind,
    pub db_fingerprint: String,
    pub model_revision: String,
    pub total_conversations: u64,
    pub conversations_in_batch: u64,
    pub last_offset: i64,
}

#[derive(Debug, Clone)]
pub struct SemanticBackfillStoragePlan {
    pub tier: TierKind,
    pub db_fingerprint: String,
    pub model_revision: String,
    pub max_conversations: usize,
}

#[derive(Debug, Clone)]
pub struct SemanticBackfillBatchOutcome {
    pub tier: TierKind,
    pub embedder_id: String,
    pub embedded_docs: u64,
    pub conversations_processed: u64,
    pub total_conversations: u64,
    pub last_offset: i64,
    pub checkpoint_saved: bool,
    pub published: bool,
    pub index_path: PathBuf,
    pub manifest_path: PathBuf,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn hnsw_index_path(data_dir: &Path, embedder_id: &str) -> PathBuf {
    data_dir
        .join(VECTOR_INDEX_DIR)
        .join(format!("hnsw-{embedder_id}.chsw"))
}

fn safe_path_component(raw: &str) -> String {
    raw.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn semantic_staging_index_path(
    data_dir: &Path,
    tier: TierKind,
    embedder_id: &str,
    db_fingerprint: &str,
) -> PathBuf {
    let fingerprint_hash = crc32fast::hash(db_fingerprint.as_bytes());
    data_dir.join(VECTOR_INDEX_DIR).join(format!(
        ".staging-{}-{}-{fingerprint_hash:08x}.fsvi",
        tier.as_str(),
        safe_path_component(embedder_id)
    ))
}

fn sync_parent_directory(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let directory = fs::File::open(parent)
        .with_context(|| format!("opening parent directory {}", parent.display()))?;
    directory
        .sync_all()
        .with_context(|| format!("syncing parent directory {}", parent.display()))
}

fn semantic_doc_id_for_embedded(embedded: &EmbeddedMessage) -> String {
    SemanticDocId {
        message_id: embedded.message_id,
        chunk_idx: embedded.chunk_idx,
        agent_id: embedded.agent_id,
        workspace_id: embedded.workspace_id,
        source_id: embedded.source_id,
        role: embedded.role,
        created_at_ms: embedded.created_at_ms,
        content_hash: Some(embedded.content_hash),
    }
    .to_doc_id_string()
}

struct CanonicalEmbeddingRow {
    conversation_id: i64,
    message_id: i64,
    created_at: Option<i64>,
    agent_id: i64,
    workspace_id: Option<i64>,
    source_id: Option<String>,
    role: String,
    content: String,
}

struct CanonicalEmbeddingBatch {
    inputs: Vec<EmbeddingInput>,
    conversations_in_batch: u64,
    last_conversation_id: i64,
    total_conversations: u64,
}

fn matching_semantic_checkpoint_offset(
    manifest: &SemanticManifest,
    tier: TierKind,
    embedder_id: &str,
    db_fingerprint: &str,
) -> i64 {
    manifest
        .checkpoint
        .as_ref()
        .filter(|checkpoint| {
            checkpoint.tier == tier
                && checkpoint.embedder_id == embedder_id
                && checkpoint.is_valid(db_fingerprint)
        })
        .map_or(0, |checkpoint| checkpoint.last_offset)
}

fn total_semantic_conversations(storage: &FrankenStorage) -> Result<u64> {
    let count: i64 = storage
        .raw()
        .query_row_map(
            "SELECT COUNT(DISTINCT m.conversation_id)
             FROM messages m
             JOIN conversations c ON c.id = m.conversation_id",
            &[] as &[ParamValue],
            |row| row.get_typed(0),
        )
        .with_context(|| "counting canonical conversations with semantic messages")?;
    Ok(u64::try_from(count.max(0)).unwrap_or(u64::MAX))
}

fn message_id_from_db(raw: i64) -> Option<u64> {
    u64::try_from(raw).ok()
}

fn saturating_u32_from_i64(raw: i64) -> u32 {
    match u32::try_from(raw) {
        Ok(value) => value,
        Err(_) if raw.is_negative() => 0,
        Err(_) => u32::MAX,
    }
}

fn embedding_input_from_row(row: CanonicalEmbeddingRow) -> Option<EmbeddingInput> {
    let Some(message_id) = message_id_from_db(row.message_id) else {
        tracing::warn!(
            raw_message_id = row.message_id,
            "skipping out-of-range id during semantic backfill"
        );
        return None;
    };
    let source_id = row.source_id.unwrap_or_else(|| "local".to_string());
    Some(EmbeddingInput {
        message_id,
        created_at_ms: row.created_at.unwrap_or(0),
        agent_id: saturating_u32_from_i64(row.agent_id),
        workspace_id: saturating_u32_from_i64(row.workspace_id.unwrap_or(0)),
        source_id: crc32fast::hash(source_id.as_bytes()),
        role: role_code_from_str(&row.role).unwrap_or(ROLE_USER),
        chunk_idx: 0,
        content: row.content,
    })
}

fn fetch_canonical_embedding_batch(
    storage: &FrankenStorage,
    after_conversation_id: i64,
    max_conversations: usize,
) -> Result<CanonicalEmbeddingBatch> {
    let total_conversations = total_semantic_conversations(storage)?;
    let max_conversations_i64 = i64::try_from(max_conversations.max(1)).unwrap_or(i64::MAX);
    let conversation_ids: Vec<i64> = storage
        .raw()
        .query_map_collect(
            "SELECT DISTINCT m.conversation_id
             FROM messages m
             JOIN conversations c ON c.id = m.conversation_id
             WHERE m.conversation_id > ?1
             ORDER BY m.conversation_id ASC
             LIMIT ?2",
            &[
                ParamValue::from(after_conversation_id),
                ParamValue::from(max_conversations_i64),
            ],
            |row| row.get_typed(0),
        )
        .with_context(|| {
            format!("fetching semantic backfill conversation ids after {after_conversation_id}")
        })?;

    if conversation_ids.is_empty() {
        return Ok(CanonicalEmbeddingBatch {
            inputs: Vec::new(),
            conversations_in_batch: 0,
            last_conversation_id: after_conversation_id,
            total_conversations,
        });
    }

    let mut sql = String::from(
        "SELECT c.id, m.id, m.created_at, COALESCE(c.agent_id, 0), c.workspace_id, c.source_id, m.role, m.content
         FROM messages m
         JOIN conversations c ON c.id = m.conversation_id
         WHERE c.id IN (",
    );
    let mut params = Vec::with_capacity(conversation_ids.len());
    for (idx, conversation_id) in conversation_ids.iter().enumerate() {
        if idx > 0 {
            sql.push_str(", ");
        }
        sql.push_str(&format!("?{}", idx + 1));
        params.push(ParamValue::from(*conversation_id));
    }
    sql.push_str(") ORDER BY c.id ASC, m.idx ASC, m.id ASC");

    let rows: Vec<CanonicalEmbeddingRow> = storage
        .raw()
        .query_map_collect(&sql, &params, |row| {
            Ok(CanonicalEmbeddingRow {
                conversation_id: row.get_typed(0)?,
                message_id: row.get_typed(1)?,
                created_at: row.get_typed(2)?,
                agent_id: row.get_typed(3)?,
                workspace_id: row.get_typed(4)?,
                source_id: row.get_typed(5)?,
                role: row.get_typed(6)?,
                content: row.get_typed(7)?,
            })
        })
        .with_context(|| {
            format!(
                "fetching semantic backfill messages for {} conversations",
                conversation_ids.len()
            )
        })?;

    let mut conversations_in_batch = 0_u64;
    let mut last_seen_conversation_id = None;
    let mut inputs = Vec::with_capacity(rows.len());
    for row in rows {
        if last_seen_conversation_id != Some(row.conversation_id) {
            conversations_in_batch = conversations_in_batch.saturating_add(1);
            last_seen_conversation_id = Some(row.conversation_id);
        }
        if let Some(input) = embedding_input_from_row(row) {
            inputs.push(input);
        }
    }

    Ok(CanonicalEmbeddingBatch {
        inputs,
        conversations_in_batch,
        last_conversation_id: *conversation_ids.last().unwrap_or(&after_conversation_id),
        total_conversations,
    })
}

struct Prepared<'a> {
    msg: &'a EmbeddingInput,
    canonical: String,
    hash: [u8; 32],
}

/// Canonicalize + hash a window of messages. Default is serial; opt in to
/// the rayon-parallel path via `CASS_SEMANTIC_PREP_PARALLEL=1` (see the
/// `parallel_prep_enabled` docstring for why it is not the default).
/// Parallel results preserve input order via `par_iter().filter_map().collect()`.
/// Messages whose canonical form is empty are filtered out so the embedder
/// batch is never polluted with useless inputs.
fn prepare_window<'a>(window: &'a [EmbeddingInput], serial: bool) -> Vec<Prepared<'a>> {
    let prep = |msg: &'a EmbeddingInput| -> Option<Prepared<'a>> {
        let canonical = canonicalize_for_embedding(&msg.content);
        if canonical.is_empty() {
            return None;
        }
        let hash = content_hash(&canonical);
        Some(Prepared {
            msg,
            canonical,
            hash,
        })
    };

    if serial {
        window.iter().filter_map(prep).collect()
    } else {
        window.par_iter().filter_map(prep).collect()
    }
}

fn flush_prepared_batch(
    batch: &[Prepared<'_>],
    embeddings: &mut Vec<EmbeddedMessage>,
    pb: &ProgressBar,
    embedder: &dyn Embedder,
) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }

    let texts: Vec<&str> = batch.iter().map(|p| p.canonical.as_str()).collect();
    let vectors = embedder
        .embed_batch_sync(&texts)
        .map_err(|e| anyhow::anyhow!("embedding failed: {e}"))?;

    if vectors.len() != batch.len() {
        bail!(
            "embedder returned {} embeddings for {} inputs",
            vectors.len(),
            batch.len()
        );
    }

    for (prepared, vector) in batch.iter().zip(vectors) {
        if vector.len() != embedder.dimension() {
            bail!(
                "embedding dimension mismatch: expected {}, got {}",
                embedder.dimension(),
                vector.len()
            );
        }
        embeddings.push(EmbeddedMessage {
            message_id: prepared.msg.message_id,
            created_at_ms: prepared.msg.created_at_ms,
            agent_id: prepared.msg.agent_id,
            workspace_id: prepared.msg.workspace_id,
            source_id: prepared.msg.source_id,
            role: prepared.msg.role,
            chunk_idx: prepared.msg.chunk_idx,
            content_hash: prepared.hash,
            embedding: vector,
        });
    }

    pb.inc(batch.len() as u64);
    Ok(())
}

pub struct SemanticIndexer {
    embedder: Box<dyn Embedder>,
    batch_size: usize,
}

impl SemanticIndexer {
    pub fn new(embedder_type: &str, data_dir: Option<&Path>) -> Result<Self> {
        let embedder: Box<dyn Embedder> = match embedder_type {
            "fastembed" => {
                let dir = data_dir
                    .ok_or_else(|| anyhow::anyhow!("data_dir required for fastembed embedder"))?;
                let model_dir = FastEmbedder::default_model_dir(dir);
                Box::new(
                    FastEmbedder::load_from_dir(&model_dir)
                        .map_err(|e| anyhow::anyhow!("fastembed unavailable: {e}"))?,
                )
            }
            "hash" => Box::new(HashEmbedder::default()),
            other => bail!("unknown embedder: {other}"),
        };

        Ok(Self {
            embedder,
            batch_size: resolved_default_batch_size(),
        })
    }

    pub fn with_batch_size(mut self, batch_size: usize) -> Result<Self> {
        if batch_size == 0 {
            bail!("batch_size must be > 0");
        }
        self.batch_size = batch_size;
        Ok(self)
    }

    pub fn batch_size(&self) -> usize {
        self.batch_size
    }

    pub fn embedder_id(&self) -> &str {
        self.embedder.id()
    }

    pub fn embedder_dimension(&self) -> usize {
        self.embedder.dimension()
    }

    pub fn embed_messages(&self, messages: &[EmbeddingInput]) -> Result<Vec<EmbeddedMessage>> {
        if messages.is_empty() {
            return Ok(Vec::new());
        }

        let show_progress = std::io::stderr().is_terminal();
        let pb = ProgressBar::new(messages.len() as u64);
        if show_progress {
            let style = ProgressStyle::default_bar()
                .template("{spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} messages embedded")
                .unwrap_or_else(|_| ProgressStyle::default_bar());
            pb.set_style(style);
        } else {
            pb.set_draw_target(ProgressDrawTarget::hidden());
        }

        let mut embeddings = Vec::with_capacity(messages.len());

        // Process the corpus in windows of ~4 batches. Within each window,
        // rayon parallelizes the canonicalize + hash prep across cores; the
        // ONNX embedder is then fed serially in `batch_size` chunks so its
        // internal thread pool stays saturated without being starved by the
        // single-threaded prep loop we had before. `with_batch_size` and
        // `resolved_default_batch_size` both guarantee `batch_size >= 1`,
        // so saturating_mul(4) is always >= batch_size — no further clamp.
        let window = self.batch_size.saturating_mul(4);
        for window_slice in messages.chunks(window) {
            let prepared_window = prepare_window(window_slice, !parallel_prep_enabled());
            let skipped_in_window = window_slice.len() - prepared_window.len();
            if skipped_in_window > 0 {
                pb.inc(skipped_in_window as u64);
            }

            for batch in prepared_window.chunks(self.batch_size) {
                flush_prepared_batch(batch, &mut embeddings, &pb, self.embedder.as_ref())?;
            }
        }

        pb.finish_with_message("Embedding complete");
        Ok(embeddings)
    }

    pub fn build_and_save_index<I>(
        &self,
        embedded_messages: I,
        data_dir: &Path,
    ) -> Result<FsVectorIndex>
    where
        I: IntoIterator<Item = EmbeddedMessage>,
    {
        let index_path = vector_index_path(data_dir, self.embedder_id());
        self.build_and_save_index_at_path(embedded_messages, &index_path)
    }

    fn build_and_save_index_at_path<I>(
        &self,
        embedded_messages: I,
        index_path: &Path,
    ) -> Result<FsVectorIndex>
    where
        I: IntoIterator<Item = EmbeddedMessage>,
    {
        if let Some(parent) = index_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Store as f16 by default (smaller, faster I/O). Embeddings are validated by the writer.
        let mut writer: FsVectorIndexWriter = FsVectorIndex::create_with_revision(
            index_path,
            self.embedder_id(),
            "1.0",
            self.embedder_dimension(),
            FsQuantization::F16,
        )
        .map_err(|err| anyhow::anyhow!("create fsvi index failed: {err}"))?;

        let write_result: Result<()> = (|| {
            for embedded in embedded_messages {
                if embedded.embedding.len() != self.embedder_dimension() {
                    bail!(
                        "embedding dimension mismatch: expected {}, got {}",
                        self.embedder_dimension(),
                        embedded.embedding.len()
                    );
                }
                let doc_id = semantic_doc_id_for_embedded(&embedded);
                writer
                    .write_record(&doc_id, &embedded.embedding)
                    .map_err(|err| anyhow::anyhow!("write fsvi record failed: {err}"))?;
            }
            Ok(())
        })();

        if let Err(e) = &write_result {
            // Clean up partial index file to prevent corruption
            tracing::warn!("removing partial vector index after write failure: {e}");
            if let Err(rm_err) = std::fs::remove_file(index_path) {
                tracing::error!(
                    "failed to remove partial index file {}: {rm_err}",
                    index_path.display()
                );
            }
            return Err(anyhow::anyhow!("{e}"));
        }

        writer
            .finish()
            .map_err(|err| anyhow::anyhow!("finish fsvi index failed: {err}"))?;

        FsVectorIndex::open(index_path)
            .map_err(|err| anyhow::anyhow!("open fsvi index failed: {err}"))
    }

    /// Append new embeddings to an existing FSVI index via the WAL.
    ///
    /// Used for incremental semantic indexing in watch mode. Opens the
    /// existing index, appends a batch of new embeddings, and compacts if
    /// the WAL has grown large enough.
    ///
    /// Returns the number of entries appended.
    pub fn append_to_index(
        &self,
        embedded_messages: impl IntoIterator<Item = EmbeddedMessage>,
        data_dir: &Path,
    ) -> Result<usize> {
        let index_path = vector_index_path(data_dir, self.embedder_id());
        self.append_to_index_path(embedded_messages, &index_path)
    }

    fn append_to_index_path(
        &self,
        embedded_messages: impl IntoIterator<Item = EmbeddedMessage>,
        index_path: &Path,
    ) -> Result<usize> {
        let mut index = FsVectorIndex::open(index_path)
            .map_err(|err| anyhow::anyhow!("open fsvi index for append: {err}"))?;

        let entries: Vec<(String, Vec<f32>)> = embedded_messages
            .into_iter()
            .map(|em| {
                let doc_id = semantic_doc_id_for_embedded(&em);
                (doc_id, em.embedding)
            })
            .collect();

        let count = entries.len();
        if count == 0 {
            return Ok(0);
        }

        index
            .append_batch(&entries)
            .map_err(|err| anyhow::anyhow!("append_batch: {err}"))?;

        if index.needs_compaction() {
            index
                .compact()
                .map_err(|err| anyhow::anyhow!("compaction: {err}"))?;
        }

        Ok(count)
    }

    fn write_backfill_staging_index(
        &self,
        embedded_messages: Vec<EmbeddedMessage>,
        staging_path: &Path,
        resume_existing: bool,
    ) -> Result<FsVectorIndex> {
        if resume_existing && staging_path.exists() {
            self.append_to_index_path(embedded_messages, staging_path)?;
            FsVectorIndex::open(staging_path)
                .map_err(|err| anyhow::anyhow!("open staged semantic index failed: {err}"))
        } else {
            self.build_and_save_index_at_path(embedded_messages, staging_path)
        }
    }

    pub fn run_backfill_batch(
        &self,
        messages: &[EmbeddingInput],
        data_dir: &Path,
        manifest: &mut SemanticManifest,
        plan: SemanticBackfillBatchPlan,
    ) -> Result<SemanticBackfillBatchOutcome> {
        if plan.db_fingerprint.trim().is_empty() {
            bail!("semantic backfill requires a non-empty DB fingerprint");
        }
        if plan.total_conversations == 0 && plan.conversations_in_batch > 0 {
            bail!("semantic backfill batch cannot process conversations when total is zero");
        }

        let manifest_path = SemanticManifest::path(data_dir);
        let staging_path = semantic_staging_index_path(
            data_dir,
            plan.tier,
            self.embedder_id(),
            &plan.db_fingerprint,
        );
        let final_path = vector_index_path(data_dir, self.embedder_id());

        let prior_checkpoint = manifest
            .checkpoint
            .as_ref()
            .filter(|checkpoint| {
                checkpoint.tier == plan.tier
                    && checkpoint.embedder_id == self.embedder_id()
                    && checkpoint.is_valid(&plan.db_fingerprint)
            })
            .cloned();
        let prior_conversations = prior_checkpoint
            .as_ref()
            .map_or(0, |checkpoint| checkpoint.conversations_processed);
        let prior_docs = prior_checkpoint
            .as_ref()
            .map_or(0, |checkpoint| checkpoint.docs_embedded);

        let embeddings = self.embed_messages(messages)?;
        let embedded_docs = u64::try_from(embeddings.len()).unwrap_or(u64::MAX);
        let staged_index = self.write_backfill_staging_index(
            embeddings,
            &staging_path,
            prior_checkpoint.is_some(),
        )?;
        let docs_embedded = u64::try_from(staged_index.record_count()).unwrap_or(u64::MAX);
        let conversations_processed = prior_conversations
            .saturating_add(plan.conversations_in_batch)
            .min(plan.total_conversations);
        let checkpoint_docs = prior_docs.saturating_add(embedded_docs).max(docs_embedded);
        let complete = conversations_processed >= plan.total_conversations;

        manifest.refresh_backlog(plan.total_conversations, &plan.db_fingerprint);

        if complete {
            let db_fingerprint = plan.db_fingerprint.clone();
            drop(staged_index);
            fs::rename(&staging_path, &final_path).with_context(|| {
                format!(
                    "publishing staged semantic index {} to {}",
                    staging_path.display(),
                    final_path.display()
                )
            })?;
            sync_parent_directory(&final_path)?;
            let published_index = FsVectorIndex::open(&final_path)
                .map_err(|err| anyhow::anyhow!("open published semantic index failed: {err}"))?;
            let size_bytes = fs::metadata(&final_path)
                .with_context(|| format!("stat published semantic index {}", final_path.display()))?
                .len();
            let relative_index_path = final_path
                .strip_prefix(data_dir)
                .unwrap_or(final_path.as_path())
                .to_string_lossy()
                .to_string();
            manifest.publish_artifact(ArtifactRecord {
                tier: plan.tier,
                embedder_id: self.embedder_id().to_string(),
                model_revision: plan.model_revision,
                schema_version: SEMANTIC_SCHEMA_VERSION,
                chunking_version: CHUNKING_STRATEGY_VERSION,
                dimension: self.embedder_dimension(),
                doc_count: u64::try_from(published_index.record_count()).unwrap_or(u64::MAX),
                conversation_count: conversations_processed,
                db_fingerprint: plan.db_fingerprint,
                index_path: relative_index_path,
                size_bytes,
                started_at_ms: prior_checkpoint
                    .as_ref()
                    .map_or_else(now_ms, |checkpoint| checkpoint.saved_at_ms),
                completed_at_ms: now_ms(),
                ready: true,
            });
            manifest.refresh_backlog(plan.total_conversations, &db_fingerprint);
            manifest.save(data_dir)?;
        } else {
            manifest.save_checkpoint(BuildCheckpoint {
                tier: plan.tier,
                embedder_id: self.embedder_id().to_string(),
                last_offset: plan.last_offset,
                docs_embedded: checkpoint_docs,
                conversations_processed,
                total_conversations: plan.total_conversations,
                db_fingerprint: plan.db_fingerprint,
                schema_version: SEMANTIC_SCHEMA_VERSION,
                chunking_version: CHUNKING_STRATEGY_VERSION,
                saved_at_ms: now_ms(),
            });
            manifest.save(data_dir)?;
        }

        Ok(SemanticBackfillBatchOutcome {
            tier: plan.tier,
            embedder_id: self.embedder_id().to_string(),
            embedded_docs,
            conversations_processed,
            total_conversations: plan.total_conversations,
            last_offset: plan.last_offset,
            checkpoint_saved: !complete,
            published: complete,
            index_path: if complete { final_path } else { staging_path },
            manifest_path,
        })
    }

    pub fn run_backfill_from_storage(
        &self,
        storage: &FrankenStorage,
        data_dir: &Path,
        manifest: &mut SemanticManifest,
        plan: SemanticBackfillStoragePlan,
    ) -> Result<SemanticBackfillBatchOutcome> {
        let after_conversation_id = matching_semantic_checkpoint_offset(
            manifest,
            plan.tier,
            self.embedder_id(),
            &plan.db_fingerprint,
        );
        let batch = fetch_canonical_embedding_batch(
            storage,
            after_conversation_id,
            plan.max_conversations,
        )?;
        self.run_backfill_batch(
            &batch.inputs,
            data_dir,
            manifest,
            SemanticBackfillBatchPlan {
                tier: plan.tier,
                db_fingerprint: plan.db_fingerprint,
                model_revision: plan.model_revision,
                total_conversations: batch.total_conversations,
                conversations_in_batch: batch.conversations_in_batch,
                last_offset: batch.last_conversation_id,
            },
        )
    }

    /// Build and save an HNSW index for approximate nearest neighbor search.
    ///
    /// This creates an HNSW graph structure from the existing VectorIndex,
    /// enabling O(log n) approximate search with the `--approximate` flag.
    ///
    /// # Arguments
    /// * `vector_index` - The VectorIndex to build HNSW from
    /// * `data_dir` - Directory to save the HNSW index
    /// * `m` - Max connections per node (default: 16)
    /// * `ef_construction` - Search width during build (default: 200)
    ///
    /// # Returns
    /// Path to the saved HNSW index file
    pub fn build_hnsw_index(
        &self,
        vector_index: &FsVectorIndex,
        data_dir: &Path,
        m: Option<usize>,
        ef_construction: Option<usize>,
    ) -> Result<PathBuf> {
        let m = m.unwrap_or(FS_HNSW_DEFAULT_M);
        let ef_construction = ef_construction.unwrap_or(FS_HNSW_DEFAULT_EF_CONSTRUCTION);

        tracing::info!(
            embedder = self.embedder_id(),
            count = vector_index.record_count(),
            m,
            ef_construction,
            "Building HNSW index for approximate nearest neighbor search"
        );

        let config = FsHnswConfig {
            m,
            ef_construction,
            ..FsHnswConfig::default()
        };
        let hnsw = FsHnswIndex::build_from_vector_index(vector_index, config)
            .map_err(|err| anyhow::anyhow!("build HNSW index failed: {err}"))?;

        let hnsw_path = hnsw_index_path(data_dir, self.embedder_id());
        if let Some(parent) = hnsw_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        hnsw.save(&hnsw_path)
            .map_err(|err| anyhow::anyhow!("save HNSW index failed: {err}"))?;

        tracing::info!(?hnsw_path, "Saved HNSW index");
        Ok(hnsw_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
    use crate::storage::sqlite::FrankenStorage;
    use serde_json::json;
    use tempfile::tempdir;

    fn test_conversation(external_id: &str, body: &str) -> Conversation {
        Conversation {
            id: None,
            agent_slug: "codex".to_string(),
            workspace: None,
            external_id: Some(external_id.to_string()),
            title: Some(format!("semantic {external_id}")),
            source_path: PathBuf::from(format!("/tmp/{external_id}.jsonl")),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_001_000),
            approx_tokens: None,
            metadata_json: json!({}),
            messages: vec![Message {
                id: None,
                idx: 0,
                role: MessageRole::User,
                author: None,
                created_at: Some(1_700_000_000_500),
                content: body.to_string(),
                extra_json: json!({}),
                snippets: Vec::new(),
            }],
            source_id: "local".to_string(),
            origin_host: None,
        }
    }

    #[test]
    fn test_batch_embedding() {
        let indexer = SemanticIndexer::new("hash", None).unwrap();
        let messages = vec![
            EmbeddingInput::new(1, "Hello world"),
            EmbeddingInput::new(2, "Goodbye world"),
        ];

        let embeddings = indexer.embed_messages(&messages).unwrap();

        assert_eq!(embeddings.len(), 2);
        assert_eq!(embeddings[0].message_id, 1);
        assert_eq!(embeddings[1].message_id, 2);
        assert_eq!(embeddings[0].embedding.len(), indexer.embedder_dimension());
    }

    #[test]
    fn test_progress_indicator() {
        let indexer = SemanticIndexer::new("hash", None).unwrap();
        let messages: Vec<_> = (0..1000)
            .map(|i| EmbeddingInput::new(i as u64, format!("Message {}", i)))
            .collect();

        let embeddings = indexer.embed_messages(&messages).unwrap();
        assert_eq!(embeddings.len(), messages.len());
    }

    #[test]
    fn test_build_and_save_index() {
        let indexer = SemanticIndexer::new("hash", None).unwrap();
        let messages = vec![
            EmbeddingInput::new(1, "Hello world"),
            EmbeddingInput::new(2, "Goodbye world"),
        ];

        let embeddings = indexer.embed_messages(&messages).unwrap();
        let tmp = tempdir().unwrap();
        let index = indexer
            .build_and_save_index(embeddings, tmp.path())
            .unwrap();
        assert_eq!(index.embedder_id(), indexer.embedder_id());
        assert_eq!(index.dimension(), indexer.embedder_dimension());
        assert_eq!(index.record_count(), 2);
    }

    /// Golden-output regression: any change to the embedding prep pipeline,
    /// the canonicalizer, the hash embedder's deterministic projection, or
    /// the ordering semantics of `embed_messages` must not silently mutate
    /// the bytes we write to the vector index. This digest is derived from a
    /// frozen 64-message corpus processed through the hash embedder; a
    /// mismatch means one of those contracts moved.
    #[test]
    fn embed_messages_golden_digest_hash_embedder() {
        use ring::digest::{Context, SHA256};

        let corpus: Vec<EmbeddingInput> = (0..64)
            .map(|i| {
                let body = match i % 5 {
                    0 => format!("plain text message number {i}"),
                    1 => format!("**bold** line {i} with _emphasis_"),
                    2 => format!("```rust\nfn f_{i}() {{ println!(\"{i}\"); }}\n```"),
                    3 => format!("   whitespace {i}   "),
                    _ => format!("unicode \u{00E9}\u{0301} + emoji \u{1F600} {i}"),
                };
                EmbeddingInput::new(i as u64, body)
            })
            .collect();

        let indexer = SemanticIndexer::new("hash", None)
            .unwrap()
            .with_batch_size(16)
            .unwrap();
        let embeddings = indexer.embed_messages(&corpus).unwrap();

        // Digest over (message_id, content_hash, embedding f32 bytes) for every
        // embedded message, in the order emitted. Preserves order + content +
        // numeric equality without having to compare raw floats directly.
        let mut ctx = Context::new(&SHA256);
        for em in &embeddings {
            ctx.update(&em.message_id.to_le_bytes());
            ctx.update(&em.content_hash);
            for v in &em.embedding {
                ctx.update(&v.to_le_bytes());
            }
        }
        let digest = hex::encode(ctx.finish().as_ref());

        // Captured 2026-04-21 against a freshly built hash embedder, batch
        // size 16, the frozen 64-message corpus above. Stable so long as
        // the prep pipeline, canonicalizer, and HashEmbedder::embed
        // implementation are all byte-preserving. If you intentionally
        // changed any of those, update this value AND record the reason
        // in the commit message.
        const EXPECTED: &str = "22d9ae7076925a4b70a194b0f519dfb1d465cc757368c296ef24055a02038c2c";
        assert_eq!(
            digest, EXPECTED,
            "embed_messages golden digest drifted; if this was intentional, \
             update EXPECTED in this test and record the reason in the commit message"
        );
    }

    #[test]
    fn parallel_prep_matches_serial_prep_bitwise() {
        // Mix of short, long, empty, markdown, code-block, and unicode inputs
        // to make sure the canonicalizer is exercised across all of its paths.
        let inputs: Vec<EmbeddingInput> = (0..500)
            .map(|i| {
                let text = match i % 7 {
                    0 => format!("Plain message number {i} with some ordinary words."),
                    1 => format!("**Bold** and _italic_ markdown line {i}"),
                    2 => format!(
                        "```rust\nfn example_{i}() {{\n    println!(\"code block {i}\");\n}}\n```\nfollow-up text"
                    ),
                    3 => String::new(), // empty — should be filtered
                    4 => format!("   whitespace   galore   {i}   "),
                    5 => format!("Unicode \u{00E9}\u{0301} (combining accent) and emoji \u{1F600} line {i}"),
                    _ => format!(
                        "Mixed line {i}: `inline_code`, [link](http://x), {{braces}}, and \u{201C}curly quotes\u{201D}."
                    ),
                };
                EmbeddingInput::new(i as u64, text)
            })
            .collect();

        let serial = prepare_window(&inputs, true);
        let parallel = prepare_window(&inputs, false);

        assert_eq!(
            serial.len(),
            parallel.len(),
            "serial and parallel prep should skip the same number of empty canonicals"
        );

        for (s, p) in serial.iter().zip(parallel.iter()) {
            assert_eq!(
                s.msg.message_id, p.msg.message_id,
                "ordering must be preserved between serial and parallel prep"
            );
            assert_eq!(
                s.canonical, p.canonical,
                "canonical form diverged between serial and parallel prep"
            );
            assert_eq!(
                s.hash, p.hash,
                "content hash diverged between serial and parallel prep"
            );
        }
    }

    #[test]
    fn parallel_prep_filters_empty_canonicals() {
        let inputs = vec![
            EmbeddingInput::new(1, "valid content"),
            EmbeddingInput::new(2, ""),
            EmbeddingInput::new(3, "   \n\n   \t  "),
            EmbeddingInput::new(4, "more valid content"),
        ];

        let prepared = prepare_window(&inputs, false);
        let ids: Vec<u64> = prepared.iter().map(|p| p.msg.message_id).collect();

        assert!(ids.contains(&1));
        assert!(ids.contains(&4));
        // ids 2 and 3 should be dropped because their canonicals are empty.
        assert!(!ids.contains(&2));
        assert!(!ids.contains(&3));
    }

    #[test]
    fn backfill_batch_saves_checkpoint_and_staged_index_until_complete() {
        let temp = tempdir().unwrap();
        let mut manifest = SemanticManifest::default();
        let indexer = SemanticIndexer::new("hash", None).unwrap();
        let messages = vec![
            EmbeddingInput::new(10, "first staged semantic message"),
            EmbeddingInput::new(11, "second staged semantic message"),
        ];

        let outcome = indexer
            .run_backfill_batch(
                &messages,
                temp.path(),
                &mut manifest,
                SemanticBackfillBatchPlan {
                    tier: TierKind::Fast,
                    db_fingerprint: "db-fp-backfill-partial".to_string(),
                    model_revision: "hash".to_string(),
                    total_conversations: 2,
                    conversations_in_batch: 1,
                    last_offset: 1,
                },
            )
            .unwrap();

        assert!(!outcome.published);
        assert!(outcome.checkpoint_saved);
        assert!(outcome.index_path.exists());
        assert!(!vector_index_path(temp.path(), indexer.embedder_id()).exists());
        let checkpoint = manifest.checkpoint.as_ref().expect("checkpoint");
        assert_eq!(checkpoint.tier, TierKind::Fast);
        assert_eq!(checkpoint.conversations_processed, 1);
        assert_eq!(checkpoint.docs_embedded, 2);
        assert_eq!(manifest.backlog.total_conversations, 2);
        assert!(SemanticManifest::path(temp.path()).exists());
    }

    #[test]
    fn backfill_batch_resumes_staged_index_and_publishes_manifest_atomically() {
        let temp = tempdir().unwrap();
        let mut manifest = SemanticManifest::default();
        let indexer = SemanticIndexer::new("hash", None).unwrap();
        let db_fingerprint = "db-fp-backfill-complete";
        let staging_path = semantic_staging_index_path(
            temp.path(),
            TierKind::Fast,
            indexer.embedder_id(),
            db_fingerprint,
        );

        let first = vec![EmbeddingInput::new(20, "first resume batch")];
        let first_outcome = indexer
            .run_backfill_batch(
                &first,
                temp.path(),
                &mut manifest,
                SemanticBackfillBatchPlan {
                    tier: TierKind::Fast,
                    db_fingerprint: db_fingerprint.to_string(),
                    model_revision: "hash".to_string(),
                    total_conversations: 2,
                    conversations_in_batch: 1,
                    last_offset: 1,
                },
            )
            .unwrap();
        assert_eq!(first_outcome.index_path, staging_path);
        assert!(staging_path.exists());

        let second = vec![EmbeddingInput::new(21, "second resume batch")];
        let second_outcome = indexer
            .run_backfill_batch(
                &second,
                temp.path(),
                &mut manifest,
                SemanticBackfillBatchPlan {
                    tier: TierKind::Fast,
                    db_fingerprint: db_fingerprint.to_string(),
                    model_revision: "hash".to_string(),
                    total_conversations: 2,
                    conversations_in_batch: 1,
                    last_offset: 2,
                },
            )
            .unwrap();

        assert!(second_outcome.published);
        assert!(!second_outcome.checkpoint_saved);
        assert!(!staging_path.exists());
        let final_path = vector_index_path(temp.path(), indexer.embedder_id());
        assert_eq!(second_outcome.index_path, final_path);
        assert!(final_path.exists());
        assert!(manifest.checkpoint.is_none());
        let artifact = manifest.fast_tier.as_ref().expect("published fast tier");
        assert!(artifact.ready);
        assert_eq!(artifact.conversation_count, 2);
        assert_eq!(artifact.doc_count, 2);
        assert_eq!(manifest.backlog.fast_tier_processed, 2);

        let loaded = SemanticManifest::load(temp.path()).unwrap().unwrap();
        assert!(loaded.checkpoint.is_none());
        assert!(loaded.fast_tier.as_ref().is_some_and(|record| record.ready));
    }

    #[test]
    fn backfill_from_storage_fetches_canonical_batches_and_resumes() -> Result<()> {
        let temp = tempdir().unwrap();
        let db_path = temp.path().join("agent_search.db");
        let storage = FrankenStorage::open(&db_path)?;
        let agent_id = storage.ensure_agent(&Agent {
            id: None,
            slug: "codex".to_string(),
            name: "Codex".to_string(),
            version: None,
            kind: AgentKind::Cli,
        })?;
        storage.insert_conversation_tree(
            agent_id,
            None,
            &test_conversation("first", "first canonical semantic message"),
        )?;
        storage.insert_conversation_tree(
            agent_id,
            None,
            &test_conversation("second", "second canonical semantic message"),
        )?;

        let mut manifest = SemanticManifest::default();
        let indexer = SemanticIndexer::new("hash", None)?;

        let first = indexer.run_backfill_from_storage(
            &storage,
            temp.path(),
            &mut manifest,
            SemanticBackfillStoragePlan {
                tier: TierKind::Fast,
                db_fingerprint: "canonical-db-fp".to_string(),
                model_revision: "hash".to_string(),
                max_conversations: 1,
            },
        )?;
        assert!(!first.published);
        assert!(first.checkpoint_saved);
        assert_eq!(first.conversations_processed, 1);
        assert_eq!(first.total_conversations, 2);
        assert_eq!(first.embedded_docs, 1);
        assert!(first.last_offset > 0);

        let second = indexer.run_backfill_from_storage(
            &storage,
            temp.path(),
            &mut manifest,
            SemanticBackfillStoragePlan {
                tier: TierKind::Fast,
                db_fingerprint: "canonical-db-fp".to_string(),
                model_revision: "hash".to_string(),
                max_conversations: 1,
            },
        )?;
        assert!(second.published);
        assert!(!second.checkpoint_saved);
        assert_eq!(second.conversations_processed, 2);
        assert_eq!(second.embedded_docs, 1);
        assert!(manifest.checkpoint.is_none());
        assert_eq!(
            manifest.fast_tier.as_ref().map(|record| record.doc_count),
            Some(2)
        );
        Ok(())
    }

    #[test]
    fn default_batch_size_uses_new_value() {
        // The test setup must not leak a caller-provided CASS_SEMANTIC_BATCH_SIZE
        // override, which would mask the constant bump we're asserting on.
        let prior = std::env::var("CASS_SEMANTIC_BATCH_SIZE").ok();
        // SAFETY: test-local env mutation.
        unsafe {
            std::env::remove_var("CASS_SEMANTIC_BATCH_SIZE");
        }
        let indexer = SemanticIndexer::new("hash", None).unwrap();
        assert_eq!(indexer.batch_size(), DEFAULT_SEMANTIC_BATCH_SIZE);
        // Restore whatever was there before.
        if let Some(v) = prior {
            // SAFETY: test-local env mutation.
            unsafe {
                std::env::set_var("CASS_SEMANTIC_BATCH_SIZE", v);
            }
        }
    }
}
