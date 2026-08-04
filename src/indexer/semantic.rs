use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use frankensearch::index::{
    HNSW_DEFAULT_EF_CONSTRUCTION as FS_HNSW_DEFAULT_EF_CONSTRUCTION,
    HNSW_DEFAULT_M as FS_HNSW_DEFAULT_M, HnswConfig as FsHnswConfig, HnswIndex as FsHnswIndex,
    Quantization as FsQuantization, VectorIndex as FsVectorIndex,
    VectorIndexWriter as FsVectorIndexWriter, wal_path_for as fsvi_wal_path_for,
};
use frankensqlite::{
    SqliteValue,
    compat::{ConnectionExt, ParamValue, RowExt},
};
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use rayon::prelude::*;

use crate::indexer::memoization::{
    ContentAddressedMemoCache, MemoCacheAuditRecord, MemoContentHash, MemoKey, MemoLookup,
};
use crate::indexer::responsiveness;
use crate::indexer::semantic_progress::{
    SemanticProgressEvent, SemanticProgressFields, SemanticProgressSink,
};
use crate::model::conversation_packet::{ConversationPacket, ConversationPacketProvenance};
use crate::model::types::{Conversation, Message};
use crate::search::canonicalize::{canonicalize_for_embedding, content_hash};
use crate::search::embedder::Embedder;
use crate::search::fastembed_embedder::{FastEmbedder, MINILM_VECTOR_SPACE_REVISION};
use crate::search::hash_embedder::HashEmbedder;
use crate::search::policy::{CHUNKING_STRATEGY_VERSION, SEMANTIC_SCHEMA_VERSION, SemanticPolicy};
use crate::search::semantic_manifest::{
    ArtifactRecord, BuildCheckpoint, SemanticManifest, SemanticShardManifest, SemanticShardRecord,
    TierKind,
};
use crate::search::tantivy::{
    normalized_index_origin_host, normalized_index_origin_kind, normalized_index_source_id,
};
use crate::search::vector_index::{
    ROLE_USER, SemanticDocId, VECTOR_INDEX_DIR, role_code_from_str, vector_index_path,
};
use crate::storage::sqlite::FrankenStorage;

/// Default embedder batch size — the **maximum row count** per embed batch.
/// 128 is a sweet spot for short messages: big enough to amortize dispatch
/// overhead and keep the tensor kernels saturated. It is no longer the sole
/// bound on a batch: [`DEFAULT_SEMANTIC_EMBED_BATCH_CHAR_BUDGET`] additionally
/// caps `row_count × max_canonical_len` so a batch that happens to contain a
/// long message holds proportionally fewer rows (cass #309).
const DEFAULT_SEMANTIC_BATCH_SIZE: usize = 128;
/// Default per-batch canonical-character budget for length-aware embed
/// batching (cass #309). A fixed-size batch (128) padded to its longest member
/// makes the ONNX embedder allocate a `batch × max_seq²` attention tensor, so a
/// single long message in the batch blows RSS to multiple GB (#309 observed
/// 4.7–9.9 GB on a 67 MB corpus, flat RSS + 1000–1200% CPU = ort inference, not
/// FSVI staging). Capping `row_count × max_canonical_len ≤ this` bounds that
/// padded working set; with `MAX_EMBED_CHARS`-capped (~2 KB) messages the worst
/// case is ≈8 rows — the regime that embeds cleanly (<1 GB) in practice. Set
/// `CASS_SEMANTIC_EMBED_BATCH_CHAR_BUDGET=0` to disable (fixed `batch_size`
/// chunks, pre-#309 behavior).
const DEFAULT_SEMANTIC_EMBED_BATCH_CHAR_BUDGET: usize = 16 * 1024;
const DEFAULT_SEMANTIC_PREP_MEMO_CAPACITY: usize = 4_096;
const DEFAULT_SEMANTIC_EMBED_BATCH_WARN_AFTER_MS: u64 = 30_000;
const DEFAULT_SEMANTIC_EMBED_BATCH_FAIL_AFTER_MS: u64 = 300_000;
const DEFAULT_SEMANTIC_MAX_MESSAGES_PER_CHECKPOINT: usize = 10_000;
const DEFAULT_SEMANTIC_MAX_BYTES_PER_CHECKPOINT: u64 = 8 * 1024 * 1024;
const DEFAULT_SEMANTIC_RECONCILIATION_SCAN_CONVERSATIONS: usize = 64;
const SEMANTIC_PREP_MEMO_ALGORITHM: &str = "semantic_prepare_window";
const SEMANTIC_PREP_MEMO_VERSION: &str = "canonicalize_for_embedding:v2:stable-content-hash";
pub const HASH_VECTOR_SPACE_REVISION: &str = "hash-fnv1a-modular-v1";

/// Return the exact vector-space revision that is safe for a current embedder.
/// ID and dimension alone are insufficient because inference-engine changes
/// can preserve both while changing vector values.
pub fn expected_vector_space_revision(embedder_id: &str) -> Option<&'static str> {
    match embedder_id {
        "minilm-384" => Some(MINILM_VECTOR_SPACE_REVISION),
        "fnv1a-384" => Some(HASH_VECTOR_SPACE_REVISION),
        _ => None,
    }
}

fn resolved_env_usize(key: &str, default: usize) -> usize {
    dotenvy::var(key)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(default)
}

fn resolved_env_u64(key: &str, default: u64) -> u64 {
    dotenvy::var(key)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default)
}

fn resolved_default_batch_size() -> usize {
    dotenvy::var("CASS_SEMANTIC_BATCH_SIZE")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_SEMANTIC_BATCH_SIZE)
}

fn resolved_semantic_prep_memo_capacity() -> usize {
    dotenvy::var("CASS_SEMANTIC_PREP_MEMO_CAPACITY")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_SEMANTIC_PREP_MEMO_CAPACITY)
}

fn resolved_semantic_embed_batch_char_budget() -> usize {
    resolved_env_usize(
        "CASS_SEMANTIC_EMBED_BATCH_CHAR_BUDGET",
        DEFAULT_SEMANTIC_EMBED_BATCH_CHAR_BUDGET,
    )
}

fn resolved_semantic_embed_batch_warn_after_ms() -> u64 {
    resolved_env_u64(
        "CASS_SEMANTIC_EMBED_BATCH_WARN_AFTER_MS",
        DEFAULT_SEMANTIC_EMBED_BATCH_WARN_AFTER_MS,
    )
}

fn resolved_semantic_embed_batch_fail_after_ms() -> u64 {
    resolved_env_u64(
        "CASS_SEMANTIC_EMBED_BATCH_FAIL_AFTER_MS",
        DEFAULT_SEMANTIC_EMBED_BATCH_FAIL_AFTER_MS,
    )
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
    env_truthy("CASS_SEMANTIC_PREP_PARALLEL")
}

fn saturating_u64_from_usize(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn saturating_u64_from_millis(value: u128) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
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
    pub cursor_exhausted: bool,
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

impl SemanticBackfillBatchOutcome {
    pub fn progress_pct(&self) -> f64 {
        if self.total_conversations == 0 {
            return if self.published { 100.0 } else { 0.0 };
        }

        let pct = (self.conversations_processed as f64 / self.total_conversations as f64) * 100.0;
        let pct = pct.min(100.0);
        if pct >= 100.0 && !self.published {
            99.0
        } else {
            pct
        }
    }
}

#[derive(Debug, Clone)]
pub struct SemanticShardBuildPlan {
    pub tier: TierKind,
    pub db_fingerprint: String,
    pub model_revision: String,
    pub total_conversations: u64,
    pub max_records_per_shard: usize,
    pub build_ann: bool,
}

#[derive(Debug, Clone)]
pub struct SemanticShardBuildOutcome {
    pub tier: TierKind,
    pub embedder_id: String,
    pub shard_count: u32,
    pub doc_count: u64,
    pub total_conversations: u64,
    pub index_paths: Vec<PathBuf>,
    pub ann_index_paths: Vec<PathBuf>,
    pub shard_manifest_path: PathBuf,
    pub complete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SemanticBackfillSchedulerState {
    Running,
    Paused,
    Disabled,
}

impl SemanticBackfillSchedulerState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Disabled => "disabled",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SemanticBackfillSchedulerReason {
    IdleBudgetAvailable,
    OperatorDisabled,
    PolicyDisabled,
    ForegroundPressure,
    LexicalRepairActive,
    CapacityBelowFloor,
    ThreadBudgetZero,
    BatchBudgetZero,
}

impl SemanticBackfillSchedulerReason {
    pub(crate) fn next_step(self) -> &'static str {
        match self {
            Self::IdleBudgetAvailable => "background semantic backfill is within idle budgets",
            Self::OperatorDisabled => {
                "background semantic backfill is disabled by CASS_SEMANTIC_BACKFILL_DISABLE"
            }
            Self::PolicyDisabled => "semantic policy disables background semantic backfill",
            Self::ForegroundPressure => {
                "foreground pressure is present; retry after the idle delay"
            }
            Self::LexicalRepairActive => "lexical repair is active; semantic backfill is yielding",
            Self::CapacityBelowFloor => {
                "machine responsiveness capacity is below the semantic backfill floor"
            }
            Self::ThreadBudgetZero => "semantic backfill thread budget is zero",
            Self::BatchBudgetZero => "semantic backfill batch budget is zero",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SemanticBackfillSchedulerSignals {
    pub foreground_pressure: bool,
    pub lexical_repair_active: bool,
    pub force: bool,
    pub operator_disabled: bool,
}

impl SemanticBackfillSchedulerSignals {
    pub(crate) fn from_env() -> Self {
        Self {
            foreground_pressure: env_truthy("CASS_SEMANTIC_BACKFILL_FOREGROUND_ACTIVE"),
            lexical_repair_active: env_truthy("CASS_SEMANTIC_BACKFILL_LEXICAL_REPAIR_ACTIVE"),
            force: env_truthy("CASS_SEMANTIC_BACKFILL_FORCE"),
            operator_disabled: env_truthy("CASS_SEMANTIC_BACKFILL_DISABLE"),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct SemanticBackfillSchedulerDecision {
    pub state: SemanticBackfillSchedulerState,
    pub reason: SemanticBackfillSchedulerReason,
    pub requested_batch_conversations: usize,
    pub scheduled_batch_conversations: usize,
    pub current_capacity_pct: u32,
    pub min_capacity_pct: u32,
    pub max_backfill_threads: usize,
    pub idle_delay_seconds: u64,
    pub chunk_timeout_seconds: u64,
    pub foreground_pressure: bool,
    pub lexical_repair_active: bool,
    pub forced: bool,
    pub next_eligible_after_ms: u64,
}

impl SemanticBackfillSchedulerDecision {
    pub(crate) fn should_run(&self) -> bool {
        matches!(self.state, SemanticBackfillSchedulerState::Running)
    }
}

fn env_truthy(key: &str) -> bool {
    dotenvy::var(key)
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn env_backfill_min_capacity_pct() -> u32 {
    dotenvy::var("CASS_SEMANTIC_BACKFILL_MIN_CAPACITY_PCT")
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
        .map(|value| value.clamp(1, 100))
        .unwrap_or(75)
}

pub(crate) fn semantic_backfill_scheduler_decision(
    policy: &SemanticPolicy,
    requested_batch_conversations: usize,
    signals: &SemanticBackfillSchedulerSignals,
) -> SemanticBackfillSchedulerDecision {
    semantic_backfill_scheduler_decision_for_capacity(
        policy,
        requested_batch_conversations,
        signals,
        responsiveness::current_capacity_pct(),
    )
}

pub(crate) fn semantic_backfill_scheduler_decision_for_capacity(
    policy: &SemanticPolicy,
    requested_batch_conversations: usize,
    signals: &SemanticBackfillSchedulerSignals,
    current_capacity_pct: u32,
) -> SemanticBackfillSchedulerDecision {
    let min_capacity_pct = env_backfill_min_capacity_pct();
    let paused_delay_ms = policy.idle_delay_seconds.saturating_mul(1000);
    let mut decision = SemanticBackfillSchedulerDecision {
        state: SemanticBackfillSchedulerState::Running,
        reason: SemanticBackfillSchedulerReason::IdleBudgetAvailable,
        requested_batch_conversations,
        scheduled_batch_conversations: requested_batch_conversations,
        current_capacity_pct: current_capacity_pct.clamp(0, 100),
        min_capacity_pct,
        max_backfill_threads: policy.max_backfill_threads,
        idle_delay_seconds: policy.idle_delay_seconds,
        chunk_timeout_seconds: policy.chunk_timeout_seconds,
        foreground_pressure: signals.foreground_pressure,
        lexical_repair_active: signals.lexical_repair_active,
        forced: signals.force,
        next_eligible_after_ms: 0,
    };

    if requested_batch_conversations == 0 {
        return stopped_scheduler_decision(
            decision,
            SemanticBackfillSchedulerState::Disabled,
            SemanticBackfillSchedulerReason::BatchBudgetZero,
            paused_delay_ms,
        );
    }
    if policy.max_backfill_threads == 0 && !signals.force {
        return stopped_scheduler_decision(
            decision,
            SemanticBackfillSchedulerState::Disabled,
            SemanticBackfillSchedulerReason::ThreadBudgetZero,
            paused_delay_ms,
        );
    }
    if signals.operator_disabled && !signals.force {
        return stopped_scheduler_decision(
            decision,
            SemanticBackfillSchedulerState::Disabled,
            SemanticBackfillSchedulerReason::OperatorDisabled,
            paused_delay_ms,
        );
    }
    if !policy.mode.should_build_semantic() && !signals.force {
        return stopped_scheduler_decision(
            decision,
            SemanticBackfillSchedulerState::Disabled,
            SemanticBackfillSchedulerReason::PolicyDisabled,
            paused_delay_ms,
        );
    }
    if signals.lexical_repair_active && !signals.force {
        return stopped_scheduler_decision(
            decision,
            SemanticBackfillSchedulerState::Paused,
            SemanticBackfillSchedulerReason::LexicalRepairActive,
            paused_delay_ms,
        );
    }
    if signals.foreground_pressure && !signals.force {
        return stopped_scheduler_decision(
            decision,
            SemanticBackfillSchedulerState::Paused,
            SemanticBackfillSchedulerReason::ForegroundPressure,
            paused_delay_ms,
        );
    }
    if current_capacity_pct < min_capacity_pct && !signals.force {
        return stopped_scheduler_decision(
            decision,
            SemanticBackfillSchedulerState::Paused,
            SemanticBackfillSchedulerReason::CapacityBelowFloor,
            paused_delay_ms,
        );
    }

    let capacity = current_capacity_pct.clamp(1, 100) as usize;
    let scaled = requested_batch_conversations.saturating_mul(capacity) / 100;
    decision.scheduled_batch_conversations = scaled.max(1).min(requested_batch_conversations);
    decision
}

fn stopped_scheduler_decision(
    mut decision: SemanticBackfillSchedulerDecision,
    state: SemanticBackfillSchedulerState,
    reason: SemanticBackfillSchedulerReason,
    next_eligible_after_ms: u64,
) -> SemanticBackfillSchedulerDecision {
    decision.state = state;
    decision.reason = reason;
    decision.scheduled_batch_conversations = 0;
    decision.next_eligible_after_ms = next_eligible_after_ms;
    decision
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

fn semantic_generation_fingerprint_component(db_fingerprint: &str) -> String {
    blake3::hash(db_fingerprint.as_bytes())
        .to_hex()
        .chars()
        .take(16)
        .collect()
}

fn semantic_shard_generation_dir(
    data_dir: &Path,
    tier: TierKind,
    embedder_id: &str,
    db_fingerprint: &str,
) -> PathBuf {
    let fingerprint_hash = semantic_generation_fingerprint_component(db_fingerprint);
    data_dir.join(VECTOR_INDEX_DIR).join("shards").join(format!(
        "{}-{}-{fingerprint_hash}",
        tier.as_str(),
        safe_path_component(embedder_id),
    ))
}

fn semantic_shard_index_path(
    data_dir: &Path,
    tier: TierKind,
    embedder_id: &str,
    db_fingerprint: &str,
    shard_index: u32,
) -> PathBuf {
    semantic_shard_generation_dir(data_dir, tier, embedder_id, db_fingerprint)
        .join(format!("shard-{shard_index:05}.fsvi"))
}

fn semantic_shard_ann_index_path(
    data_dir: &Path,
    tier: TierKind,
    embedder_id: &str,
    db_fingerprint: &str,
    shard_index: u32,
) -> PathBuf {
    semantic_shard_generation_dir(data_dir, tier, embedder_id, db_fingerprint)
        .join(format!("shard-{shard_index:05}.chsw"))
}

#[cfg(not(windows))]
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

#[cfg(windows)]
fn sync_parent_directory(_path: &Path) -> Result<()> {
    Ok(())
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

pub(crate) fn semantic_doc_id_for_input(input: &EmbeddingInput) -> Option<String> {
    let canonical = canonicalize_for_embedding(&input.content);
    if canonical.is_empty() {
        return None;
    }
    Some(
        SemanticDocId {
            message_id: input.message_id,
            chunk_idx: input.chunk_idx,
            agent_id: input.agent_id,
            workspace_id: input.workspace_id,
            source_id: input.source_id,
            role: input.role,
            created_at_ms: input.created_at_ms,
            content_hash: Some(content_hash(&canonical)),
        }
        .to_doc_id_string(),
    )
}

#[cfg(not(windows))]
fn publish_reconciled_semantic_index(staging_path: &Path, final_path: &Path) -> Result<()> {
    fs::rename(staging_path, final_path).with_context(|| {
        format!(
            "publishing reconciled semantic index {} to {}",
            staging_path.display(),
            final_path.display()
        )
    })?;
    sync_parent_directory(final_path)
}

#[cfg(windows)]
fn publish_reconciled_semantic_index(staging_path: &Path, final_path: &Path) -> Result<()> {
    if !final_path.exists() {
        fs::rename(staging_path, final_path).with_context(|| {
            format!(
                "publishing reconciled semantic index {} to {}",
                staging_path.display(),
                final_path.display()
            )
        })?;
        return sync_parent_directory(final_path);
    }

    let extension = final_path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("fsvi");
    let backup_path = final_path.with_extension(format!(
        "{extension}.reconcile-backup-{}-{}",
        std::process::id(),
        now_ms()
    ));
    fs::rename(final_path, &backup_path).with_context(|| {
        format!(
            "staging existing semantic index {} at {} before replacement",
            final_path.display(),
            backup_path.display()
        )
    })?;
    match fs::rename(staging_path, final_path) {
        Ok(()) => {
            if let Err(err) = fs::remove_file(&backup_path) {
                tracing::warn!(
                    backup_path = %backup_path.display(),
                    error = %err,
                    "reconciled semantic publish left a recoverable prior-live backup"
                );
            }
            sync_parent_directory(final_path)
        }
        Err(publish_err) => match fs::rename(&backup_path, final_path) {
            Ok(()) => {
                sync_parent_directory(final_path)?;
                Err(publish_err).with_context(|| {
                    format!(
                        "failed publishing reconciled semantic index {}; restored prior live artifact",
                        final_path.display()
                    )
                })
            }
            Err(restore_err) => bail!(
                "failed publishing reconciled semantic index {}; also failed to restore prior live artifact from {}: {}; staged candidate remains at {}",
                final_path.display(),
                backup_path.display(),
                restore_err,
                staging_path.display()
            ),
        },
    }
}

struct CanonicalEmbeddingConversationRow {
    conversation_id: i64,
    agent_slug: String,
    agent_id: i64,
    workspace: Option<PathBuf>,
    workspace_id: Option<i64>,
    external_id: Option<String>,
    title: Option<String>,
    source_path: PathBuf,
    started_at: Option<i64>,
    ended_at: Option<i64>,
    source_id: Option<String>,
    origin_host: Option<String>,
}

struct CanonicalEmbeddingBatch {
    inputs: Vec<EmbeddingInput>,
    conversations_in_batch: u64,
    last_conversation_id: i64,
    total_conversations: u64,
    cursor_exhausted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SemanticCheckpointCaps {
    max_messages: usize,
    max_bytes: u64,
}

impl SemanticCheckpointCaps {
    fn unlimited() -> Self {
        Self {
            max_messages: 0,
            max_bytes: 0,
        }
    }

    fn from_env() -> Self {
        Self {
            max_messages: resolved_env_usize(
                "CASS_SEMANTIC_MAX_MESSAGES_PER_CHECKPOINT",
                DEFAULT_SEMANTIC_MAX_MESSAGES_PER_CHECKPOINT,
            ),
            max_bytes: resolved_env_u64(
                "CASS_SEMANTIC_MAX_BYTES_PER_CHECKPOINT",
                DEFAULT_SEMANTIC_MAX_BYTES_PER_CHECKPOINT,
            ),
        }
    }

    fn message_limited(self) -> bool {
        self.max_messages > 0
    }

    fn byte_limited(self) -> bool {
        self.max_bytes > 0
    }
}

pub(crate) struct CanonicalIncrementalEmbeddingBatch {
    pub inputs: Vec<EmbeddingInput>,
    pub conversations_in_batch: u64,
    pub raw_max_message_id: Option<i64>,
}

fn semantic_hot_tail_ids(storage: &FrankenStorage) -> Result<HashSet<i64>> {
    storage
        .raw()
        .query_map_collect(
            "SELECT conversation_id
             FROM conversation_tail_state
             WHERE last_message_idx IS NOT NULL
             ORDER BY conversation_id ASC",
            &[] as &[ParamValue],
            |row| row.get_typed(0),
        )
        .with_context(|| "listing semantic conversations from hot tail metadata")
        .map(|ids: Vec<i64>| ids.into_iter().collect())
}

fn semantic_conversation_has_message_after(
    storage: &FrankenStorage,
    conversation_id: i64,
    after_message_id: Option<i64>,
) -> Result<bool> {
    let mut params = vec![SqliteValue::from(conversation_id)];
    let message_cursor_predicate = if let Some(after_message_id) = after_message_id {
        params.push(SqliteValue::from(after_message_id));
        " AND id > ?2"
    } else {
        ""
    };
    let hinted_probe = format!(
        "SELECT 1
         FROM messages INDEXED BY sqlite_autoindex_messages_1
         WHERE conversation_id = ?1{message_cursor_predicate}
         LIMIT 1"
    );
    let fallback_probe = format!(
        "SELECT 1
         FROM messages
         WHERE conversation_id = ?1{message_cursor_predicate}
         LIMIT 1"
    );
    let mut has_message = false;
    storage
        .raw()
        .query_with_params_for_each(&hinted_probe, &params, |_| {
            has_message = true;
            Ok(())
        })
        .or_else(|err| {
            if !err
                .to_string()
                .contains("no such index: sqlite_autoindex_messages_1")
            {
                return Err(err);
            }
            storage
                .raw()
                .query_with_params_for_each(&fallback_probe, &params, |_| {
                    has_message = true;
                    Ok(())
                })
        })
        .with_context(|| {
            let cursor = after_message_id.map_or_else(|| "none".to_string(), |id| id.to_string());
            format!(
                "probing messages for conversation {conversation_id} after message cursor {cursor}"
            )
        })?;
    Ok(has_message)
}

fn total_semantic_conversations(storage: &FrankenStorage) -> Result<u64> {
    // `conversation_tail_state.last_message_idx` is the maintained, schema-v18
    // hot cache for every normal non-empty conversation. Merge that compact
    // table with the legacy parent-row cache in Rust, then resolve only the
    // residual legacy/empty rows through point probes on the canonical
    // `(conversation_id, idx)` autoindex. Keeping the conversation id set in
    // memory is cheap (roughly one integer per conversation) and, critically,
    // never materializes message rows or bodies.
    //
    // This avoids all three hostile FrankenSQLite 0.1.13 paths found while
    // reproducing #343: correlated EXISTS under COUNT(*) misses the join-loop
    // memo; COUNT(DISTINCT ...) uses a linear seen-set; and GROUP BY retains
    // every source row in per-group vectors.
    let hot_tail_ids = semantic_hot_tail_ids(storage)?;
    let mut count = 0_u64;
    let mut missing_tail_ids = Vec::new();
    storage
        .raw()
        .query_with_params_for_each(
            "SELECT id, last_message_idx
             FROM conversations
             ORDER BY id ASC",
            &[] as &[SqliteValue],
            |row| {
                let conversation_id: i64 = row.get_typed(0)?;
                let legacy_last_message_idx: Option<i64> = row.get_typed(1)?;
                if legacy_last_message_idx.is_some() || hot_tail_ids.contains(&conversation_id) {
                    count = count.saturating_add(1);
                } else {
                    missing_tail_ids.push(conversation_id);
                }
                Ok(())
            },
        )
        .with_context(|| "listing conversations and legacy tail metadata")?;

    for conversation_id in missing_tail_ids {
        if semantic_conversation_has_message_after(storage, conversation_id, None)? {
            count = count.saturating_add(1);
        }
    }
    Ok(count)
}

fn fetch_bounded_semantic_candidate_conversation_ids(
    storage: &FrankenStorage,
    after_conversation_id: i64,
    after_message_id: Option<i64>,
    max_candidates: usize,
) -> Result<(Vec<i64>, usize)> {
    let max_candidates = max_candidates.max(1);
    if let Some(after_message_id) = after_message_id {
        // Resume from the canonical global message cursor, not from the much
        // wider parent-table gap.  The previous implementation paged
        // `conversations` and executed one prepared message probe for every
        // parent.  A legitimate sparse checkpoint in #348 crossed 6,668
        // ineligible parents and retained more than 2 GiB before finding two
        // candidates.
        //
        // `messages.id` is the canonical INTEGER PRIMARY KEY cursor.  Force a
        // table/rowid traversal so FrankenSQLite can stream every eligible
        // post-cursor message through one statement.  A bounded BTreeSet keeps
        // only the smallest `max_candidates` conversation IDs.  This final
        // reconciliation is essential: messages may be appended to older
        // conversations, so global message-id order is not necessarily
        // conversation-id order, while the durable conversation checkpoint
        // must advance in ascending order without skipping an older candidate.
        let mut selected = BTreeSet::new();
        let mut rows_scanned = 0_usize;
        storage
            .raw()
            .query_with_params_for_each(
                "SELECT conversation_id
                 FROM messages NOT INDEXED
                 WHERE id > ?1 AND conversation_id > ?2",
                &[
                    SqliteValue::from(after_message_id),
                    SqliteValue::from(after_conversation_id),
                ],
                |row| {
                    let conversation_id: i64 = row.get_typed(0)?;
                    rows_scanned = rows_scanned.saturating_add(1);
                    if selected.contains(&conversation_id) {
                        return Ok(());
                    }
                    if selected.len() < max_candidates {
                        selected.insert(conversation_id);
                        return Ok(());
                    }
                    if selected
                        .last()
                        .is_some_and(|largest| conversation_id < *largest)
                    {
                        selected.insert(conversation_id);
                        if let Some(largest) = selected.last().copied() {
                            selected.remove(&largest);
                        }
                    }
                    Ok(())
                },
            )
            .with_context(|| {
                format!(
                    "streaming semantic candidates after conversation {after_conversation_id} and message {after_message_id}"
                )
            })?;
        return Ok((selected.into_iter().collect(), rows_scanned));
    }

    // Scan the narrow parent table in bounded pages, then issue an indexed
    // point probe for the message predicate. A correlated EXISTS looks tidy,
    // but FrankenSQLite 0.1.13's file-backed path clone/substitutes and executes
    // that subquery per outer row; on #343's 432k-message corpus it consumed
    // minutes and GiBs. Explicit probes make both bounds visible: at most one
    // small parent page resident and at most `max_candidates` selected IDs.
    let page_size = max_candidates.saturating_mul(4).clamp(256, 4_096);
    let page_size_i64 = i64::try_from(page_size).unwrap_or(i64::MAX);
    let hot_tail_ids = if after_message_id.is_none() {
        Some(semantic_hot_tail_ids(storage)?)
    } else {
        None
    };
    let mut selected = Vec::with_capacity(max_candidates.min(4_096));
    let mut rows_scanned = 0_usize;
    let mut page_cursor = after_conversation_id;

    loop {
        let page: Vec<(i64, Option<i64>)> = storage
            .raw()
            .query_map_collect(
                "SELECT id, last_message_idx
                 FROM conversations
                 WHERE id > ?1
                 ORDER BY id ASC
                 LIMIT ?2",
                &[
                    ParamValue::from(page_cursor),
                    ParamValue::from(page_size_i64),
                ],
                |row| Ok((row.get_typed(0)?, row.get_typed(1)?)),
            )
            .with_context(|| {
                format!("listing bounded semantic candidate page after conversation {page_cursor}")
            })?;
        if page.is_empty() {
            break;
        }
        let page_len = page.len();
        page_cursor = page
            .last()
            .map_or(page_cursor, |(conversation_id, _)| *conversation_id);

        for (conversation_id, legacy_last_message_idx) in page {
            rows_scanned = rows_scanned.saturating_add(1);
            let has_message = if let Some(hot_tail_ids) = hot_tail_ids.as_ref() {
                if legacy_last_message_idx.is_some() || hot_tail_ids.contains(&conversation_id) {
                    true
                } else {
                    semantic_conversation_has_message_after(storage, conversation_id, None)?
                }
            } else {
                semantic_conversation_has_message_after(storage, conversation_id, after_message_id)?
            };
            if has_message {
                selected.push(conversation_id);
                if selected.len() >= max_candidates {
                    return Ok((selected, rows_scanned));
                }
            }
        }

        if page_len < page_size {
            break;
        }
    }

    Ok((selected, rows_scanned))
}

fn cached_semantic_total_conversations(
    manifest: &SemanticManifest,
    tier: TierKind,
    embedder_id: &str,
    db_fingerprint: &str,
) -> Option<(u64, &'static str)> {
    manifest
        .checkpoint
        .as_ref()
        .filter(|checkpoint| {
            checkpoint.tier == tier
                && checkpoint.embedder_id == embedder_id
                && checkpoint.is_valid(db_fingerprint)
        })
        .map(|checkpoint| (checkpoint.total_conversations, "checkpoint"))
}

pub(crate) fn message_id_from_db(raw: i64) -> Option<u64> {
    u64::try_from(raw).ok()
}

pub(crate) fn saturating_u32_from_i64(raw: i64) -> u32 {
    match u32::try_from(raw) {
        Ok(value) => value,
        Err(_) if raw.is_negative() => 0,
        Err(_) => u32::MAX,
    }
}

fn canonical_embedding_created_at_ms(message_id: u64, created_at: Option<i64>) -> i64 {
    // `created_at_ms` feeds time-range filters in the vector index
    // (src/search/vector_index.rs range predicates) and contributes to
    // `stable_hit_hash`. Defaulting a NULL created_at to 0 silently
    // masquerades the message as Unix-epoch (1970), which is indistinguishable
    // from a legitimately-ancient row in downstream filters. Emit a warn
    // so operators see NULL-created_at rows in the logs instead of only
    // finding them by puzzling over 1970 timestamps in semantic hits.
    created_at.unwrap_or_else(|| {
        tracing::warn!(
            message_id,
            "semantic backfill: row has NULL created_at; defaulting to 0 (Unix epoch). \
             Downstream time-range filters will treat this message as 1970-01-01."
        );
        0
    })
}

fn canonical_embedding_packet_provenance(
    row: &CanonicalEmbeddingConversationRow,
) -> ConversationPacketProvenance {
    let source_id =
        normalized_index_source_id(row.source_id.as_deref(), None, row.origin_host.as_deref());
    ConversationPacketProvenance {
        origin_kind: normalized_index_origin_kind(&source_id, None),
        origin_host: normalized_index_origin_host(row.origin_host.as_deref()),
        source_id,
    }
}

fn canonical_embedding_conversation(
    row: &CanonicalEmbeddingConversationRow,
    provenance: &ConversationPacketProvenance,
    messages: Vec<Message>,
) -> Conversation {
    Conversation {
        id: Some(row.conversation_id),
        agent_slug: row.agent_slug.clone(),
        workspace: row.workspace.clone(),
        external_id: row.external_id.clone(),
        title: row.title.clone(),
        source_path: row.source_path.clone(),
        started_at: row.started_at,
        ended_at: row.ended_at,
        approx_tokens: None,
        metadata_json: serde_json::Value::Null,
        messages,
        source_id: provenance.source_id.clone(),
        origin_host: provenance.origin_host.clone(),
    }
}

fn embedding_input_from_packet_message(
    conversation_id: i64,
    agent_id: u32,
    workspace_id: u32,
    source_id_hash: u32,
    message: &crate::model::conversation_packet::ConversationPacketMessage,
) -> Option<EmbeddingInput> {
    let Some(raw_message_id) = message.message_id else {
        tracing::warn!(
            conversation_id,
            message_idx = message.idx,
            "skipping semantic backfill message without canonical id in ConversationPacket replay"
        );
        return None;
    };
    let Some(message_id) = message_id_from_db(raw_message_id) else {
        tracing::warn!(
            conversation_id,
            raw_message_id,
            "skipping out-of-range id during semantic backfill"
        );
        return None;
    };
    Some(EmbeddingInput {
        message_id,
        created_at_ms: canonical_embedding_created_at_ms(message_id, message.created_at),
        agent_id,
        workspace_id,
        source_id: source_id_hash,
        role: role_code_from_str(&message.role).unwrap_or(ROLE_USER),
        chunk_idx: 0,
        content: message.content.clone(),
    })
}

fn embedding_inputs_from_conversation_packet(
    row: &CanonicalEmbeddingConversationRow,
    packet: &ConversationPacket,
) -> Vec<EmbeddingInput> {
    let agent_id = saturating_u32_from_i64(row.agent_id);
    let workspace_id = saturating_u32_from_i64(row.workspace_id.unwrap_or(0));
    let source_id_hash = crc32fast::hash(packet.payload.provenance.source_id.as_bytes());
    packet
        .projections
        .semantic
        .message_indices
        .iter()
        .filter_map(|message_index| {
            packet
                .payload
                .messages
                .get(*message_index)
                .and_then(|message| {
                    embedding_input_from_packet_message(
                        row.conversation_id,
                        agent_id,
                        workspace_id,
                        source_id_hash,
                        message,
                    )
                })
        })
        .collect()
}

fn fetch_canonical_embedding_conversations(
    storage: &FrankenStorage,
    conversation_ids: &[i64],
) -> Result<Vec<CanonicalEmbeddingConversationRow>> {
    let mut envelope_sql = String::from(
        "SELECT c.id,
                COALESCE(a.slug, 'unknown'),
                COALESCE(c.agent_id, 0),
                c.workspace_id,
                w.path,
                c.external_id,
                c.title,
                c.source_path,
                c.started_at,
                c.ended_at,
                c.source_id,
                c.origin_host
         FROM conversations c
         LEFT JOIN agents a ON a.id = c.agent_id
         LEFT JOIN workspaces w ON w.id = c.workspace_id
         WHERE c.id IN (",
    );
    let mut params = Vec::with_capacity(conversation_ids.len());
    for (idx, conversation_id) in conversation_ids.iter().enumerate() {
        if idx > 0 {
            envelope_sql.push_str(", ");
        }
        envelope_sql.push_str(&format!("?{}", idx + 1));
        params.push(ParamValue::from(*conversation_id));
    }
    envelope_sql.push_str(") ORDER BY c.id ASC");

    storage
        .raw()
        .query_map_collect(&envelope_sql, &params, |row| {
            let workspace_path: Option<String> = row.get_typed(4)?;
            Ok(CanonicalEmbeddingConversationRow {
                conversation_id: row.get_typed(0)?,
                agent_slug: row.get_typed(1)?,
                agent_id: row.get_typed(2)?,
                workspace_id: row.get_typed(3)?,
                workspace: workspace_path.map(PathBuf::from),
                external_id: row.get_typed(5)?,
                title: row.get_typed(6)?,
                source_path: PathBuf::from(row.get_typed::<String>(7)?),
                started_at: row.get_typed(8)?,
                ended_at: row.get_typed(9)?,
                source_id: row.get_typed(10)?,
                origin_host: row.get_typed(11)?,
            })
        })
        .with_context(|| {
            format!(
                "fetching semantic backfill conversation envelopes for {} conversations",
                conversation_ids.len()
            )
        })
}

/// Per-packet semantic context that supplies the database-internal
/// agent / workspace ids the canonical embedding row carries but the
/// `ConversationPacket` does not (those ids are storage-internal,
/// not part of the packet contract).
///
/// `coding_agent_session_search-ibuuh.32` (sink #3): when a caller
/// already holds packets (rebuild pipeline, salvage replay, repair
/// flows, etc.) it can pair them with their canonical
/// agent_id/workspace_id and drive the semantic preparation consumer
/// without a second storage round-trip.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct SemanticPacketContext {
    pub conversation_id: i64,
    pub agent_id: u32,
    pub workspace_id: u32,
}

/// Packet-driven counterpart to
/// [`packet_embedding_inputs_from_storage_with_progress`]: derives the same
/// `EmbeddingInput` list a fresh storage replay would produce, but
/// without re-querying canonical conversation rows.
///
/// Invariants:
/// - The `i`th element of `contexts` describes the `i`th packet.
/// - Returns `Err` if the lengths disagree, so a callsite cannot
///   silently mis-correlate packets and contexts.
/// - `source_id_hash` is derived from `packet.payload.provenance.source_id`
///   the same way `embedding_inputs_from_conversation_packet` derives
///   it from the canonical row, so the produced `EmbeddingInput.source_id`
///   matches both paths byte-for-byte.
///
/// The `semantic_inputs_from_packets_matches_storage_replay`
/// equivalence test pins every produced `EmbeddingInput` field is
/// identical to what the legacy storage-side replay returns for the
/// same canonical corpus, so callers that already hold packets can
/// switch to this helper without changing semantic-index output.
#[allow(dead_code)]
pub(crate) fn semantic_inputs_from_packets(
    packets: &[ConversationPacket],
    contexts: &[SemanticPacketContext],
) -> Result<Vec<EmbeddingInput>> {
    if packets.len() != contexts.len() {
        anyhow::bail!(
            "semantic_inputs_from_packets length mismatch: {} packets vs {} contexts",
            packets.len(),
            contexts.len()
        );
    }
    let mut inputs = Vec::new();
    for (packet, context) in packets.iter().zip(contexts.iter()) {
        let source_id_hash = crc32fast::hash(packet.payload.provenance.source_id.as_bytes());
        for &message_index in &packet.projections.semantic.message_indices {
            let Some(message) = packet.payload.messages.get(message_index) else {
                anyhow::bail!(
                    "packet semantic projection references missing message index {} \
                     (packet has {} messages)",
                    message_index,
                    packet.payload.messages.len()
                );
            };
            if let Some(input) = embedding_input_from_packet_message(
                context.conversation_id,
                context.agent_id,
                context.workspace_id,
                source_id_hash,
                message,
            ) {
                inputs.push(input);
            }
        }
    }
    tracing::debug!(
        packets = packets.len(),
        packet_driven = true,
        semantic_inputs = inputs.len(),
        "built semantic inputs from in-memory ConversationPacket batch"
    );
    Ok(inputs)
}

#[cfg(test)]
fn fetch_canonical_embedding_batch(
    storage: &FrankenStorage,
    after_conversation_id: i64,
    max_conversations: usize,
) -> Result<CanonicalEmbeddingBatch> {
    fetch_canonical_embedding_batch_inner(storage, after_conversation_id, max_conversations, None)
}

/// Variant of [`fetch_canonical_embedding_batch`] that additionally
/// filters out canonical messages with `message_id <= after_message_id`
/// when set. This is how sub-fix 2 (`last_message_id` cursor) enforces
/// the "resume MUST advance past `last_message_id`" rule on a partially
/// embedded conversation.
#[cfg(test)]
fn fetch_canonical_embedding_batch_inner(
    storage: &FrankenStorage,
    after_conversation_id: i64,
    max_conversations: usize,
    after_message_id: Option<i64>,
) -> Result<CanonicalEmbeddingBatch> {
    fetch_canonical_embedding_batch_inner_with_caps(
        storage,
        after_conversation_id,
        max_conversations,
        after_message_id,
        SemanticCheckpointCaps::unlimited(),
    )
}

#[cfg(test)]
fn fetch_canonical_embedding_batch_inner_with_caps(
    storage: &FrankenStorage,
    after_conversation_id: i64,
    max_conversations: usize,
    after_message_id: Option<i64>,
    caps: SemanticCheckpointCaps,
) -> Result<CanonicalEmbeddingBatch> {
    let total_conversations = total_semantic_conversations(storage)?;
    fetch_canonical_embedding_batch_inner_with_caps_and_total(
        storage,
        after_conversation_id,
        max_conversations,
        after_message_id,
        caps,
        total_conversations,
        None,
    )
}

fn fetch_canonical_embedding_batch_inner_with_caps_and_total(
    storage: &FrankenStorage,
    after_conversation_id: i64,
    max_conversations: usize,
    after_message_id: Option<i64>,
    caps: SemanticCheckpointCaps,
    total_conversations: u64,
    sink: Option<&SemanticProgressSink>,
) -> Result<CanonicalEmbeddingBatch> {
    let max_conversations = max_conversations.max(1);
    let query_limit = max_conversations.saturating_add(1);
    if let Some(sink) = sink
        && sink.is_active()
    {
        sink.emit(
            SemanticProgressEvent::SelectionCandidatesStart,
            SemanticProgressFields {
                batch_rows: Some(saturating_u64_from_usize(query_limit)),
                rows_total: Some(saturating_u64_from_usize(max_conversations)),
                last_conversation_id: Some(after_conversation_id),
                last_message_id: after_message_id,
                ..Default::default()
            },
        );
    }

    let (mut conversation_ids, candidate_rows_scanned) =
        fetch_bounded_semantic_candidate_conversation_ids(
            storage,
            after_conversation_id,
            after_message_id,
            query_limit,
        )?;

    let candidate_rows_returned = conversation_ids.len();
    let has_more_from_candidate_limit = conversation_ids.len() > max_conversations;
    if has_more_from_candidate_limit {
        conversation_ids.truncate(max_conversations);
    }
    if let Some(sink) = sink
        && sink.is_active()
    {
        sink.emit(
            SemanticProgressEvent::SelectionCandidatesDone,
            SemanticProgressFields {
                batch_rows: Some(saturating_u64_from_usize(conversation_ids.len())),
                rows_processed: Some(saturating_u64_from_usize(candidate_rows_scanned)),
                rows_total: Some(saturating_u64_from_usize(max_conversations)),
                last_conversation_id: Some(
                    conversation_ids
                        .last()
                        .copied()
                        .unwrap_or(after_conversation_id),
                ),
                last_message_id: after_message_id,
                note: Some(format!(
                    "has_more={has_more_from_candidate_limit};eligible_rows={candidate_rows_returned}"
                )),
                ..Default::default()
            },
        );
    }

    if conversation_ids.is_empty() {
        return Ok(CanonicalEmbeddingBatch {
            inputs: Vec::new(),
            conversations_in_batch: 0,
            last_conversation_id: after_conversation_id,
            total_conversations,
            cursor_exhausted: true,
        });
    }

    let conversations = fetch_canonical_embedding_conversations(storage, &conversation_ids)?;

    let mut grouped_messages =
        storage.fetch_messages_for_lexical_rebuild_batch(&conversation_ids, None, None)?;
    let CheckpointCappedSelection {
        conversations,
        last_conversation_id,
        stopped_before_candidate,
    } = select_checkpoint_capped_conversations(
        conversations,
        &mut grouped_messages,
        after_message_id,
        caps,
    );
    let (inputs, _) = packet_embedding_inputs_from_materialized_canonical_messages(
        &conversations,
        &mut grouped_messages,
        |_| true,
    );

    let conversations_in_batch = u64::try_from(conversations.len()).unwrap_or(u64::MAX);
    let cursor_exhausted = !has_more_from_candidate_limit && !stopped_before_candidate;
    tracing::debug!(
        conversations_in_batch,
        cursor_exhausted,
        packet_driven = true,
        semantic_inputs = inputs.len(),
        max_messages_per_checkpoint = caps.max_messages,
        max_bytes_per_checkpoint = caps.max_bytes,
        ?after_message_id,
        "built semantic backfill batch from ConversationPacket canonical replay"
    );

    Ok(CanonicalEmbeddingBatch {
        inputs,
        conversations_in_batch,
        last_conversation_id: last_conversation_id.unwrap_or(after_conversation_id),
        total_conversations,
        cursor_exhausted,
    })
}

struct CheckpointCappedSelection {
    conversations: Vec<CanonicalEmbeddingConversationRow>,
    last_conversation_id: Option<i64>,
    stopped_before_candidate: bool,
}

fn select_checkpoint_capped_conversations(
    conversations: Vec<CanonicalEmbeddingConversationRow>,
    grouped_messages: &mut HashMap<i64, Vec<Message>>,
    after_message_id: Option<i64>,
    caps: SemanticCheckpointCaps,
) -> CheckpointCappedSelection {
    let mut selected = Vec::new();
    let mut selected_messages = 0usize;
    let mut selected_bytes = 0u64;
    let mut last_conversation_id = None;
    let mut stopped_before_candidate = false;

    for conversation in conversations {
        let mut messages = grouped_messages
            .remove(&conversation.conversation_id)
            .unwrap_or_default();
        if let Some(min_exclusive) = after_message_id {
            messages.retain(|message| message.id.is_some_and(|id| id > min_exclusive));
        }
        if messages.is_empty() {
            continue;
        }

        let message_count = messages.len();
        let byte_count = messages
            .iter()
            .map(|message| saturating_u64_from_usize(message.content.len()))
            .fold(0u64, u64::saturating_add);
        let would_exceed_messages = caps.message_limited()
            && !selected.is_empty()
            && selected_messages.saturating_add(message_count) > caps.max_messages;
        let would_exceed_bytes = caps.byte_limited()
            && !selected.is_empty()
            && selected_bytes.saturating_add(byte_count) > caps.max_bytes;
        if would_exceed_messages || would_exceed_bytes {
            stopped_before_candidate = true;
            tracing::debug!(
                conversation_id = conversation.conversation_id,
                selected_conversations = selected.len(),
                selected_messages,
                selected_bytes,
                candidate_messages = message_count,
                candidate_bytes = byte_count,
                max_messages_per_checkpoint = caps.max_messages,
                max_bytes_per_checkpoint = caps.max_bytes,
                "semantic checkpoint cap stopped this batch before the next full conversation"
            );
            break;
        }

        selected_messages = selected_messages.saturating_add(message_count);
        selected_bytes = selected_bytes.saturating_add(byte_count);
        last_conversation_id = Some(conversation.conversation_id);
        grouped_messages.insert(conversation.conversation_id, messages);
        selected.push(conversation);
    }

    CheckpointCappedSelection {
        conversations: selected,
        last_conversation_id,
        stopped_before_candidate,
    }
}

#[cfg(test)]
pub(crate) fn packet_embedding_inputs_from_storage(
    storage: &FrankenStorage,
) -> Result<Vec<EmbeddingInput>> {
    Ok(fetch_canonical_embedding_batch(storage, 0, usize::MAX)?.inputs)
}

/// Collect every canonical semantic input while reporting bounded replay
/// progress in conversation units.
///
/// The direct `cass index --semantic` path ultimately needs the complete input
/// vector for embedding, but replaying the canonical database can itself take
/// long enough to deserve an honest phase counter. Reuse the same bounded
/// visitor as reconciliation so callers see progress after each fully
/// materialized conversation batch rather than one synthetic jump after an
/// unbounded query.
pub(crate) fn packet_embedding_inputs_from_storage_with_progress<F>(
    storage: &FrankenStorage,
    on_progress: F,
) -> Result<Vec<EmbeddingInput>>
where
    F: FnMut(usize, usize),
{
    let mut inputs = Vec::new();
    visit_packet_embedding_inputs_from_storage_with_limits_and_progress(
        storage,
        DEFAULT_SEMANTIC_RECONCILIATION_SCAN_CONVERSATIONS,
        SemanticCheckpointCaps {
            max_messages: DEFAULT_SEMANTIC_MAX_MESSAGES_PER_CHECKPOINT,
            max_bytes: DEFAULT_SEMANTIC_MAX_BYTES_PER_CHECKPOINT,
        },
        |input| {
            inputs.push(input);
            Ok(())
        },
        on_progress,
    )?;
    Ok(inputs)
}

/// Replay canonical semantic inputs in bounded conversation batches.
///
/// Reconciliation needs to inspect every canonical document identity, but it
/// must not retain every message body merely to discover a small repair delta.
/// The callback therefore receives each input by value while this helper keeps
/// only one bounded replay batch resident at a time.
pub(crate) fn visit_packet_embedding_inputs_from_storage<F>(
    storage: &FrankenStorage,
    visit: F,
) -> Result<()>
where
    F: FnMut(EmbeddingInput) -> Result<()>,
{
    visit_packet_embedding_inputs_from_storage_with_limits(
        storage,
        DEFAULT_SEMANTIC_RECONCILIATION_SCAN_CONVERSATIONS,
        SemanticCheckpointCaps {
            max_messages: DEFAULT_SEMANTIC_MAX_MESSAGES_PER_CHECKPOINT,
            max_bytes: DEFAULT_SEMANTIC_MAX_BYTES_PER_CHECKPOINT,
        },
        visit,
    )
}

fn visit_packet_embedding_inputs_from_storage_with_limits<F>(
    storage: &FrankenStorage,
    max_conversations: usize,
    caps: SemanticCheckpointCaps,
    visit: F,
) -> Result<()>
where
    F: FnMut(EmbeddingInput) -> Result<()>,
{
    visit_packet_embedding_inputs_from_storage_with_limits_and_progress(
        storage,
        max_conversations,
        caps,
        visit,
        |_, _| {},
    )
}

fn visit_packet_embedding_inputs_from_storage_with_limits_and_progress<F, P>(
    storage: &FrankenStorage,
    max_conversations: usize,
    caps: SemanticCheckpointCaps,
    mut visit: F,
    mut on_progress: P,
) -> Result<()>
where
    F: FnMut(EmbeddingInput) -> Result<()>,
    P: FnMut(usize, usize),
{
    let mut after_conversation_id = 0i64;
    let total_conversations = total_semantic_conversations(storage)?;
    let total_conversations_usize = usize::try_from(total_conversations).unwrap_or(usize::MAX);
    let mut processed_conversations = 0_u64;
    loop {
        let batch = fetch_canonical_embedding_batch_inner_with_caps_and_total(
            storage,
            after_conversation_id,
            max_conversations,
            None,
            caps,
            total_conversations,
            None,
        )?;
        for input in batch.inputs {
            visit(input)?;
        }
        processed_conversations =
            processed_conversations.saturating_add(batch.conversations_in_batch);
        on_progress(
            usize::try_from(processed_conversations).unwrap_or(usize::MAX),
            total_conversations_usize,
        );
        if batch.cursor_exhausted {
            return Ok(());
        }
        if batch.last_conversation_id <= after_conversation_id {
            bail!(
                "canonical semantic reconciliation scan did not advance beyond conversation \
                 {after_conversation_id}"
            );
        }
        after_conversation_id = batch.last_conversation_id;
    }
}

fn packet_embedding_inputs_from_selected_canonical_messages<F>(
    storage: &FrankenStorage,
    conversation_ids: &[i64],
    include_message: F,
) -> Result<(Vec<EmbeddingInput>, Option<i64>)>
where
    F: FnMut(&Message) -> bool,
{
    if conversation_ids.is_empty() {
        return Ok((Vec::new(), None));
    }

    let conversations = fetch_canonical_embedding_conversations(storage, conversation_ids)?;
    let mut grouped_messages =
        storage.fetch_messages_for_lexical_rebuild_batch(conversation_ids, None, None)?;
    Ok(
        packet_embedding_inputs_from_materialized_canonical_messages(
            &conversations,
            &mut grouped_messages,
            include_message,
        ),
    )
}

fn packet_embedding_inputs_from_materialized_canonical_messages<F>(
    conversations: &[CanonicalEmbeddingConversationRow],
    grouped_messages: &mut HashMap<i64, Vec<Message>>,
    mut include_message: F,
) -> (Vec<EmbeddingInput>, Option<i64>)
where
    F: FnMut(&Message) -> bool,
{
    let mut inputs = Vec::new();
    let mut raw_max_message_id: Option<i64> = None;

    for conversation in conversations {
        let mut messages = grouped_messages
            .remove(&conversation.conversation_id)
            .unwrap_or_default();
        messages.retain(|message| {
            let keep = include_message(message);
            if keep && let Some(message_id) = message.id {
                raw_max_message_id =
                    Some(raw_max_message_id.map_or(message_id, |current| current.max(message_id)));
            }
            keep
        });
        if messages.is_empty() {
            continue;
        }

        let provenance = canonical_embedding_packet_provenance(conversation);
        let canonical = canonical_embedding_conversation(conversation, &provenance, messages);
        let packet = ConversationPacket::from_canonical_replay(&canonical, provenance);
        inputs.extend(embedding_inputs_from_conversation_packet(
            conversation,
            &packet,
        ));
    }

    (inputs, raw_max_message_id)
}

pub(crate) fn packet_embedding_inputs_from_storage_since(
    storage: &FrankenStorage,
    since_message_id: i64,
) -> Result<CanonicalIncrementalEmbeddingBatch> {
    let conversation_ids: Vec<i64> = storage
        .raw()
        .query_map_collect(
            "SELECT DISTINCT m.conversation_id
             FROM messages m
             WHERE m.id > ?1
             ORDER BY m.conversation_id ASC",
            &[ParamValue::from(since_message_id)],
            |row| row.get_typed(0),
        )
        .with_context(|| {
            format!(
                "fetching canonical semantic catch-up conversation ids after message {since_message_id}"
            )
        })?;

    if conversation_ids.is_empty() {
        return Ok(CanonicalIncrementalEmbeddingBatch {
            inputs: Vec::new(),
            conversations_in_batch: 0,
            raw_max_message_id: None,
        });
    }

    let (inputs, raw_max_message_id) = packet_embedding_inputs_from_selected_canonical_messages(
        storage,
        &conversation_ids,
        |message| message.id.is_some_and(|id| id > since_message_id),
    )?;

    let conversations_in_batch = u64::try_from(conversation_ids.len()).unwrap_or(u64::MAX);
    tracing::debug!(
        since_message_id,
        conversations_in_batch,
        packet_driven = true,
        semantic_inputs = inputs.len(),
        "built semantic catch-up batch from ConversationPacket canonical replay"
    );

    Ok(CanonicalIncrementalEmbeddingBatch {
        inputs,
        conversations_in_batch,
        raw_max_message_id,
    })
}

pub(crate) fn packet_embedding_inputs_from_storage_for_message_ids(
    storage: &FrankenStorage,
    conversation_ids: &[i64],
    message_ids: &HashSet<i64>,
) -> Result<Vec<EmbeddingInput>> {
    if conversation_ids.is_empty() || message_ids.is_empty() {
        return Ok(Vec::new());
    }

    let (inputs, raw_max_message_id) = packet_embedding_inputs_from_selected_canonical_messages(
        storage,
        conversation_ids,
        |message| message.id.is_some_and(|id| message_ids.contains(&id)),
    )?;
    tracing::debug!(
        conversations_in_batch = conversation_ids.len(),
        selected_message_ids = message_ids.len(),
        semantic_inputs = inputs.len(),
        raw_max_message_id,
        packet_driven = true,
        "built selected semantic batch from ConversationPacket canonical replay"
    );

    Ok(inputs)
}

struct Prepared<'a> {
    msg: &'a EmbeddingInput,
    canonical: String,
    hash: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MemoizedPreparedMessage {
    canonical: String,
    hash: [u8; 32],
}

fn semantic_prep_memo_key(content: &str) -> MemoKey {
    MemoKey::new(
        MemoContentHash::from_bytes(content_hash(content).to_vec()),
        SEMANTIC_PREP_MEMO_ALGORITHM,
        SEMANTIC_PREP_MEMO_VERSION,
    )
}

fn memo_counter_delta(after: u64, before: u64) -> u64 {
    after.saturating_sub(before)
}

fn trace_semantic_prep_memo_window(
    window_index: usize,
    window_len: usize,
    prepared_len: usize,
    entry_capacity: usize,
    before: &crate::indexer::memoization::MemoCacheStats,
    after: &crate::indexer::memoization::MemoCacheStats,
) {
    tracing::trace!(
        algorithm = SEMANTIC_PREP_MEMO_ALGORITHM,
        algorithm_version = SEMANTIC_PREP_MEMO_VERSION,
        window_index,
        window_len,
        prepared_messages = prepared_len,
        skipped_messages = window_len.saturating_sub(prepared_len),
        hit_delta = memo_counter_delta(after.hits, before.hits),
        miss_delta = memo_counter_delta(after.misses, before.misses),
        insert_delta = memo_counter_delta(after.inserts, before.inserts),
        evictions_capacity_delta =
            memo_counter_delta(after.evictions_capacity, before.evictions_capacity),
        quarantined_delta = memo_counter_delta(after.quarantined, before.quarantined),
        live_entries = after.live_entries,
        entry_capacity,
        "semantic prep memo cache window"
    );
}

fn trace_semantic_prep_memo_audit(audit: &MemoCacheAuditRecord) {
    tracing::trace!(?audit, "semantic prep memo cache audit");
}

fn prepare_window_with_memo<'a>(
    window: &'a [EmbeddingInput],
    cache: &mut ContentAddressedMemoCache<MemoizedPreparedMessage>,
) -> Vec<Prepared<'a>> {
    window
        .iter()
        .filter_map(|msg| {
            let key = semantic_prep_memo_key(&msg.content);
            let (lookup, lookup_audit) = cache.get_with_audit(&key);
            trace_semantic_prep_memo_audit(&lookup_audit);
            match lookup {
                MemoLookup::Hit { value } => Some(Prepared {
                    msg,
                    canonical: value.canonical,
                    hash: value.hash,
                }),
                MemoLookup::Miss | MemoLookup::Quarantined { .. } => {
                    let canonical = canonicalize_for_embedding(&msg.content);
                    if canonical.is_empty() {
                        return None;
                    }
                    let hash = content_hash(&canonical);
                    let insert_audit = cache.insert_with_audit(
                        key,
                        MemoizedPreparedMessage {
                            canonical: canonical.clone(),
                            hash,
                        },
                    );
                    trace_semantic_prep_memo_audit(&insert_audit);
                    Some(Prepared {
                        msg,
                        canonical,
                        hash,
                    })
                }
            }
        })
        .collect()
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

/// Split a prepared window into contiguous, length-aware embed batches (cass
/// #309).
///
/// Each returned batch holds at most `max_count` rows AND keeps
/// `row_count × max_canonical_len ≤ char_budget`, so a batch that contains a
/// long message stays small and the embedder's padded `batch × max_seq²`
/// working set stays bounded regardless of corpus content. A single message
/// longer than `char_budget` becomes its own one-row batch (never dropped).
/// Order is preserved — callers still zip results back to inputs positionally.
/// `char_budget == 0` disables the byte bound (fixed `max_count` chunks,
/// pre-#309 behavior).
fn length_aware_batches<'p, 'a>(
    prepared: &'p [Prepared<'a>],
    max_count: usize,
    char_budget: usize,
) -> Vec<&'p [Prepared<'a>]> {
    let max_count = max_count.max(1);
    let mut batches: Vec<&'p [Prepared<'a>]> = Vec::new();
    if prepared.is_empty() {
        return batches;
    }
    let mut start = 0usize;
    let mut max_len = 0usize;
    for (i, item) in prepared.iter().enumerate() {
        let len = item.canonical.len();
        let count = i - start + 1;
        let prospective_max = max_len.max(len);
        // Keep at least one row per batch: only split on the budget once the
        // in-progress batch already holds a row (`count > 1`), so a lone
        // over-budget message still forms its own batch rather than looping.
        let over_budget =
            char_budget > 0 && count > 1 && count.saturating_mul(prospective_max) > char_budget;
        if count > max_count || over_budget {
            batches.push(&prepared[start..i]);
            start = i;
            max_len = len;
        } else {
            max_len = prospective_max;
        }
    }
    batches.push(&prepared[start..]);
    batches
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
        validate_embedding_vector(&vector, embedder.dimension(), prepared.msg.message_id)?;
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

    pb.inc(saturating_u64_from_usize(batch.len()));
    Ok(())
}

fn validate_embedding_vector(
    vector: &[f32],
    expected_dimension: usize,
    message_id: u64,
) -> Result<()> {
    if vector.len() != expected_dimension {
        bail!(
            "embedding dimension mismatch: expected {}, got {}",
            expected_dimension,
            vector.len()
        );
    }
    if vector.iter().any(|value| !value.is_finite()) {
        bail!("embedding for message {message_id} contains a non-finite value");
    }
    Ok(())
}

pub struct SemanticIndexer {
    embedder: Box<dyn Embedder>,
    batch_size: usize,
}

impl SemanticIndexer {
    pub fn new(embedder_type: &str, data_dir: Option<&Path>) -> Result<Self> {
        let embedder: Box<dyn Embedder> = match embedder_type {
            "fastembed" | "minilm" => {
                let dir = data_dir
                    .ok_or_else(|| anyhow::anyhow!("data_dir required for fastembed embedder"))?;
                let embedder_name = if embedder_type == "fastembed" {
                    "minilm"
                } else {
                    embedder_type
                };
                Box::new(
                    FastEmbedder::load_by_name(dir, embedder_name)
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

    fn vector_space_revision(&self) -> Result<&'static str> {
        expected_vector_space_revision(self.embedder_id()).ok_or_else(|| {
            anyhow::anyhow!(
                "no vector-space revision registered for embedder {}",
                self.embedder_id()
            )
        })
    }

    pub fn embed_messages(&self, messages: &[EmbeddingInput]) -> Result<Vec<EmbeddedMessage>> {
        self.embed_messages_with_sink(messages, &SemanticProgressSink::disabled())
    }

    /// Embed messages while reporting how many input rows have been fully
    /// handled. Rows rejected during canonical preparation count as handled so
    /// the caller's phase counter always reaches its declared total.
    pub(crate) fn embed_messages_with_progress<F>(
        &self,
        messages: &[EmbeddingInput],
        on_progress: F,
    ) -> Result<Vec<EmbeddedMessage>>
    where
        F: FnMut(usize, usize),
    {
        self.embed_messages_with_sink_and_progress(
            messages,
            &SemanticProgressSink::disabled(),
            on_progress,
        )
    }

    /// Variant of [`embed_messages`] that emits `embed_batch_*` events
    /// into the given JSONL sink. The sink is silent unless
    /// `CASS_SEMANTIC_PROGRESS_JSONL` is set, so this path is safe to
    /// take in production.
    pub fn embed_messages_with_sink(
        &self,
        messages: &[EmbeddingInput],
        sink: &SemanticProgressSink,
    ) -> Result<Vec<EmbeddedMessage>> {
        self.embed_messages_with_sink_and_progress(messages, sink, |_, _| {})
    }

    fn embed_messages_with_sink_and_progress<F>(
        &self,
        messages: &[EmbeddingInput],
        sink: &SemanticProgressSink,
        mut on_progress: F,
    ) -> Result<Vec<EmbeddedMessage>>
    where
        F: FnMut(usize, usize),
    {
        if messages.is_empty() {
            on_progress(0, 0);
            return Ok(Vec::new());
        }

        let show_progress = std::io::stderr().is_terminal();
        let pb = ProgressBar::new(saturating_u64_from_usize(messages.len()));
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
        let serial_prep = !parallel_prep_enabled();
        let prep_memo_capacity = resolved_semantic_prep_memo_capacity();
        let mut prep_memo =
            serial_prep.then(|| ContentAddressedMemoCache::with_capacity(prep_memo_capacity));
        let mut batch_index: u64 = 0;
        let mut rows_processed: u64 = 0;
        let rows_total = u64::try_from(messages.len()).ok();
        let warn_after_ms = resolved_semantic_embed_batch_warn_after_ms();
        let fail_after_ms = resolved_semantic_embed_batch_fail_after_ms();
        // cass #309: bound `row_count × max_canonical_len` per embed batch so a
        // long message can't inflate a fixed-128 batch's padded tensor to
        // multiple GB. Resolved once per call (not per batch).
        let embed_char_budget = resolved_semantic_embed_batch_char_budget();
        for (window_index, window_slice) in messages.chunks(window).enumerate() {
            let prepared_window = match prep_memo.as_mut() {
                Some(cache) => {
                    let stats_before = cache.stats().clone();
                    let prepared_window = prepare_window_with_memo(window_slice, cache);
                    trace_semantic_prep_memo_window(
                        window_index,
                        window_slice.len(),
                        prepared_window.len(),
                        prep_memo_capacity,
                        &stats_before,
                        cache.stats(),
                    );
                    prepared_window
                }
                None => prepare_window(window_slice, false),
            };
            let skipped_in_window = window_slice.len() - prepared_window.len();
            if skipped_in_window > 0 {
                pb.inc(saturating_u64_from_usize(skipped_in_window));
                rows_processed =
                    rows_processed.saturating_add(saturating_u64_from_usize(skipped_in_window));
                on_progress(
                    usize::try_from(rows_processed).unwrap_or(usize::MAX),
                    messages.len(),
                );
            }

            for batch in length_aware_batches(&prepared_window, self.batch_size, embed_char_budget)
            {
                let batch_rows = u64::try_from(batch.len()).unwrap_or(u64::MAX);
                // Sum the canonicalized byte count so an operator can
                // distinguish a stalled inference from a stalled query —
                // a tiny `bytes` value paired with a long batch wall-time
                // points at the model; a huge `bytes` paired with a short
                // wall-time points at the storage side.
                let batch_bytes: u64 = batch
                    .iter()
                    .map(|p| saturating_u64_from_usize(p.canonical.len()))
                    .sum();
                if sink.is_active() {
                    sink.emit(
                        SemanticProgressEvent::EmbedBatchStart,
                        SemanticProgressFields {
                            batch_index: Some(batch_index),
                            batch_rows: Some(batch_rows),
                            rows_processed: Some(rows_processed),
                            rows_total,
                            bytes: Some(batch_bytes),
                            ..Default::default()
                        },
                    );
                }
                let batch_started = Instant::now();
                flush_prepared_batch(batch, &mut embeddings, &pb, self.embedder.as_ref())?;
                let elapsed_ms = saturating_u64_from_millis(batch_started.elapsed().as_millis());
                rows_processed = rows_processed.saturating_add(batch_rows);
                on_progress(
                    usize::try_from(rows_processed).unwrap_or(usize::MAX),
                    messages.len(),
                );
                if warn_after_ms > 0 && elapsed_ms > warn_after_ms {
                    tracing::warn!(
                        batch_index,
                        elapsed_ms,
                        warn_after_ms,
                        batch_rows,
                        batch_bytes,
                        embedder = self.embedder_id(),
                        "semantic embed batch exceeded watchdog warning threshold"
                    );
                }
                if fail_after_ms > 0 && elapsed_ms > fail_after_ms {
                    bail!(
                        "semantic embed batch {batch_index} took {elapsed_ms}ms, exceeding \
                         CASS_SEMANTIC_EMBED_BATCH_FAIL_AFTER_MS={fail_after_ms}"
                    );
                }
                if sink.is_active() {
                    sink.emit(
                        SemanticProgressEvent::EmbedBatchDone,
                        SemanticProgressFields {
                            batch_index: Some(batch_index),
                            batch_rows: Some(batch_rows),
                            rows_processed: Some(rows_processed),
                            rows_total,
                            bytes: Some(batch_bytes),
                            ..Default::default()
                        },
                    );
                }
                batch_index = batch_index.saturating_add(1);
            }
        }

        if let Some(cache) = prep_memo.as_ref() {
            let stats = cache.stats();
            tracing::debug!(
                algorithm = SEMANTIC_PREP_MEMO_ALGORITHM,
                algorithm_version = SEMANTIC_PREP_MEMO_VERSION,
                hits = stats.hits,
                misses = stats.misses,
                inserts = stats.inserts,
                quarantined = stats.quarantined,
                live_entries = stats.live_entries,
                entry_capacity = prep_memo_capacity,
                "semantic prep memo cache summary"
            );
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

    /// Build the direct vector index and report each record only after its
    /// validated write succeeds.
    pub(crate) fn build_and_save_index_with_progress<I, F>(
        &self,
        embedded_messages: I,
        data_dir: &Path,
        on_progress: F,
    ) -> Result<FsVectorIndex>
    where
        I: IntoIterator<Item = EmbeddedMessage>,
        F: FnMut(usize),
    {
        let index_path = vector_index_path(data_dir, self.embedder_id());
        self.build_and_save_index_at_path_with_progress(embedded_messages, &index_path, on_progress)
    }

    pub fn build_and_save_index_shards<I>(
        &self,
        embedded_messages: I,
        data_dir: &Path,
        plan: SemanticShardBuildPlan,
    ) -> Result<SemanticShardBuildOutcome>
    where
        I: IntoIterator<Item = EmbeddedMessage>,
    {
        if plan.db_fingerprint.trim().is_empty() {
            bail!("semantic shard build requires a non-empty DB fingerprint");
        }
        if plan.max_records_per_shard == 0 {
            bail!("semantic shard build requires max_records_per_shard > 0");
        }

        let mut shard_records = Vec::new();
        let mut index_paths = Vec::new();
        let mut ann_index_paths = Vec::new();
        let mut current_records = Vec::with_capacity(plan.max_records_per_shard);
        let mut shard_index = 0u32;
        let mut total_docs = 0u64;

        for embedded in embedded_messages {
            current_records.push(embedded);
            if current_records.len() >= plan.max_records_per_shard {
                let records = std::mem::take(&mut current_records);
                let (record, path, ann_path) =
                    self.write_semantic_shard(records, data_dir, &plan, shard_index)?;
                total_docs = total_docs.saturating_add(record.doc_count);
                shard_records.push(record);
                index_paths.push(path);
                if let Some(path) = ann_path {
                    ann_index_paths.push(path);
                }
                shard_index = shard_index
                    .checked_add(1)
                    .context("semantic shard index overflow")?;
            }
        }

        if !current_records.is_empty() {
            let records = std::mem::take(&mut current_records);
            let (record, path, ann_path) =
                self.write_semantic_shard(records, data_dir, &plan, shard_index)?;
            total_docs = total_docs.saturating_add(record.doc_count);
            shard_records.push(record);
            index_paths.push(path);
            if let Some(path) = ann_path {
                ann_index_paths.push(path);
            }
        }

        let shard_count = u32::try_from(shard_records.len())
            .context("semantic shard generation exceeded u32 shard count")?;
        for record in &mut shard_records {
            record.shard_count = shard_count;
        }

        let mut shard_manifest = SemanticShardManifest::load_or_default(data_dir)
            .map_err(|err| anyhow::anyhow!("loading semantic shard manifest for publish: {err}"))?;
        shard_manifest.replace_shards_for_generation(
            plan.tier,
            self.embedder_id(),
            &plan.db_fingerprint,
            shard_records,
        );
        shard_manifest
            .save(data_dir)
            .map_err(|err| anyhow::anyhow!("saving semantic shard manifest: {err}"))?;
        let summary = shard_manifest.summary(plan.tier, self.embedder_id(), &plan.db_fingerprint);

        tracing::info!(
            tier = plan.tier.as_str(),
            embedder = self.embedder_id(),
            shard_count,
            doc_count = total_docs,
            total_conversations = plan.total_conversations,
            "published semantic shard generation sidecar"
        );

        Ok(SemanticShardBuildOutcome {
            tier: plan.tier,
            embedder_id: self.embedder_id().to_string(),
            shard_count,
            doc_count: total_docs,
            total_conversations: plan.total_conversations,
            index_paths,
            ann_index_paths,
            shard_manifest_path: SemanticShardManifest::path(data_dir),
            complete: summary.complete,
        })
    }

    fn write_semantic_shard(
        &self,
        embedded_messages: Vec<EmbeddedMessage>,
        data_dir: &Path,
        plan: &SemanticShardBuildPlan,
        shard_index: u32,
    ) -> Result<(SemanticShardRecord, PathBuf, Option<PathBuf>)> {
        let started_at_ms = now_ms();
        let shard_path = semantic_shard_index_path(
            data_dir,
            plan.tier,
            self.embedder_id(),
            &plan.db_fingerprint,
            shard_index,
        );
        let shard_index_file = self.build_and_save_index_at_path(embedded_messages, &shard_path)?;
        let size_bytes = fs::metadata(&shard_path)
            .with_context(|| format!("stat semantic shard {}", shard_path.display()))?
            .len();
        let (ann_index_path, ann_size_bytes, ann_ready, ann_absolute_path) = if plan.build_ann {
            let ann_path = semantic_shard_ann_index_path(
                data_dir,
                plan.tier,
                self.embedder_id(),
                &plan.db_fingerprint,
                shard_index,
            );
            let config = FsHnswConfig {
                m: FS_HNSW_DEFAULT_M,
                ef_construction: FS_HNSW_DEFAULT_EF_CONSTRUCTION,
                ..FsHnswConfig::default()
            };
            let hnsw = FsHnswIndex::build_from_vector_index(&shard_index_file, config)
                .map_err(|err| anyhow::anyhow!("build shard HNSW index failed: {err}"))?;
            hnsw.save(&ann_path)
                .map_err(|err| anyhow::anyhow!("save shard HNSW index failed: {err}"))?;
            let ann_size_bytes = fs::metadata(&ann_path)
                .with_context(|| format!("stat semantic shard ANN {}", ann_path.display()))?
                .len();
            let relative_ann_path = ann_path
                .strip_prefix(data_dir)
                .unwrap_or(ann_path.as_path())
                .to_string_lossy()
                .to_string();
            (
                Some(relative_ann_path),
                ann_size_bytes,
                true,
                Some(ann_path),
            )
        } else {
            (None, 0, false, None)
        };
        let relative_index_path = shard_path
            .strip_prefix(data_dir)
            .unwrap_or(shard_path.as_path())
            .to_string_lossy()
            .to_string();
        let record = SemanticShardRecord {
            tier: plan.tier,
            embedder_id: self.embedder_id().to_string(),
            model_revision: plan.model_revision.clone(),
            schema_version: SEMANTIC_SCHEMA_VERSION,
            chunking_version: CHUNKING_STRATEGY_VERSION,
            dimension: self.embedder_dimension(),
            shard_index,
            shard_count: 0,
            doc_count: u64::try_from(shard_index_file.record_count()).unwrap_or(u64::MAX),
            total_conversations: plan.total_conversations,
            db_fingerprint: plan.db_fingerprint.clone(),
            index_path: relative_index_path,
            quantization: "f16".to_string(),
            mmap_ready: true,
            ann_index_path,
            ann_size_bytes,
            ann_ready,
            size_bytes,
            started_at_ms,
            completed_at_ms: now_ms(),
            ready: true,
        };
        Ok((record, shard_path, ann_absolute_path))
    }

    fn build_and_save_index_at_path<I>(
        &self,
        embedded_messages: I,
        index_path: &Path,
    ) -> Result<FsVectorIndex>
    where
        I: IntoIterator<Item = EmbeddedMessage>,
    {
        self.build_and_save_index_at_path_with_progress(embedded_messages, index_path, |_| {})
    }

    fn build_and_save_index_at_path_with_progress<I, F>(
        &self,
        embedded_messages: I,
        index_path: &Path,
        mut on_progress: F,
    ) -> Result<FsVectorIndex>
    where
        I: IntoIterator<Item = EmbeddedMessage>,
        F: FnMut(usize),
    {
        if let Some(parent) = index_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Store as f16 by default (smaller, faster I/O). Validate before the
        // writer so NaN/Inf never reaches a persisted or staged artifact.
        let mut writer: FsVectorIndexWriter = FsVectorIndex::create_with_revision(
            index_path,
            self.embedder_id(),
            self.vector_space_revision()?,
            self.embedder_dimension(),
            FsQuantization::F16,
        )
        .map_err(|err| anyhow::anyhow!("create fsvi index failed: {err}"))?;

        let mut records_written = 0_usize;
        let write_result: Result<()> = (|| {
            for embedded in embedded_messages {
                validate_embedding_vector(
                    &embedded.embedding,
                    self.embedder_dimension(),
                    embedded.message_id,
                )?;
                let doc_id = semantic_doc_id_for_embedded(&embedded);
                writer
                    .write_record(&doc_id, &embedded.embedding)
                    .map_err(|err| anyhow::anyhow!("write fsvi record failed: {err}"))?;
                records_written = records_written.saturating_add(1);
                on_progress(records_written);
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

        let expected_revision = self.vector_space_revision()?;
        if index.embedder_id() != self.embedder_id()
            || index.dimension() != self.embedder_dimension()
            || index.embedder_revision() != expected_revision
        {
            bail!(
                "semantic index is incompatible with current vector space: expected embedder {} revision {} dimension {}, found embedder {} revision {} dimension {}; rebuild semantic vectors before appending",
                self.embedder_id(),
                expected_revision,
                self.embedder_dimension(),
                index.embedder_id(),
                index.embedder_revision(),
                index.dimension()
            );
        }

        let entries: Vec<(String, Vec<f32>)> = embedded_messages
            .into_iter()
            .map(|em| {
                validate_embedding_vector(&em.embedding, self.embedder_dimension(), em.message_id)?;
                let doc_id = semantic_doc_id_for_embedded(&em);
                Ok((doc_id, em.embedding))
            })
            .collect::<Result<_>>()?;

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

    /// Rebuild a canonical semantic artifact by copying exact still-current
    /// vectors and writing only newly embedded or changed documents.
    ///
    /// The live FSVI is not replaced until the candidate contains exactly the
    /// current canonical document identities and every vector has been read
    /// back successfully. The live FSVI and WAL are copied into a private
    /// same-filesystem staging directory before compaction, so reconciliation
    /// never mutates the published artifact before the atomic candidate swap.
    pub fn reconcile_index_with_canonical_documents(
        &self,
        embedded_messages: Vec<EmbeddedMessage>,
        data_dir: &Path,
        tier: TierKind,
        db_fingerprint: &str,
        current_doc_ids: &HashSet<String>,
    ) -> Result<FsVectorIndex> {
        let index_path = vector_index_path(data_dir, self.embedder_id());

        let mut replacement_doc_ids = HashSet::with_capacity(embedded_messages.len());
        for embedded in &embedded_messages {
            if embedded.embedding.len() != self.embedder_dimension() {
                bail!(
                    "embedding dimension mismatch: expected {}, got {}",
                    self.embedder_dimension(),
                    embedded.embedding.len()
                );
            }
            if embedded.embedding.iter().any(|value| !value.is_finite()) {
                bail!("semantic reconciliation replacement contains a non-finite embedding");
            }
            let doc_id = semantic_doc_id_for_embedded(embedded);
            if !current_doc_ids.contains(&doc_id) {
                bail!(
                    "semantic reconciliation replacement is not present in the canonical DB: {doc_id}"
                );
            }
            if !replacement_doc_ids.insert(doc_id.clone()) {
                bail!("semantic reconciliation received duplicate replacement document: {doc_id}");
            }
        }

        let index_parent = index_path.parent().with_context(|| {
            format!(
                "semantic reconciliation index path has no parent: {}",
                index_path.display()
            )
        })?;
        fs::create_dir_all(index_parent)?;
        let temp_prefix = format!(
            ".reconcile-{}-{}-{:08x}-",
            tier.as_str(),
            safe_path_component(self.embedder_id()),
            crc32fast::hash(db_fingerprint.as_bytes())
        );
        let staging_dir = tempfile::Builder::new()
            .prefix(&temp_prefix)
            .tempdir_in(index_parent)
            .with_context(|| {
                format!(
                    "create semantic reconciliation staging directory in {}",
                    index_parent.display()
                )
            })?;
        let staging_path = staging_dir.path().join("candidate.fsvi");
        fs::copy(&index_path, &staging_path).with_context(|| {
            format!(
                "snapshot live semantic index {} at {}",
                index_path.display(),
                staging_path.display()
            )
        })?;
        let live_wal_path = fsvi_wal_path_for(&index_path);
        let live_wal_snapshot_path = staging_dir.path().join("live.wal.snapshot");
        if live_wal_path.exists() {
            fs::copy(&live_wal_path, &live_wal_snapshot_path).with_context(|| {
                format!(
                    "snapshot live semantic WAL {} at {}",
                    live_wal_path.display(),
                    live_wal_snapshot_path.display()
                )
            })?;
            fs::copy(&live_wal_snapshot_path, fsvi_wal_path_for(&staging_path)).with_context(
                || {
                    format!(
                        "attach semantic WAL snapshot to candidate {}",
                        staging_path.display()
                    )
                },
            )?;
        }

        let mut staged = FsVectorIndex::open(&staging_path).map_err(|err| {
            anyhow::anyhow!(
                "open semantic reconciliation snapshot {}: {err}",
                staging_path.display()
            )
        })?;
        if staged.embedder_id() != self.embedder_id()
            || staged.dimension() != self.embedder_dimension()
            || staged.embedder_revision() != self.vector_space_revision()?
        {
            bail!(
                "semantic reconciliation snapshot is incompatible with embedder {} revision {} dimension {}",
                self.embedder_id(),
                self.vector_space_revision()?,
                self.embedder_dimension()
            );
        }
        if staged.wal_record_count() > 0 {
            staged.compact().map_err(|err| {
                anyhow::anyhow!("compact semantic reconciliation snapshot: {err}")
            })?;
        }
        if staged.wal_record_count() > 0 {
            bail!("semantic reconciliation could not clear the snapshot FSVI WAL");
        }

        let initial_tombstones = staged.tombstone_count();
        let mut stale_doc_ids = HashSet::new();
        for record_index in 0..staged.record_count() {
            if staged.is_deleted(record_index) {
                continue;
            }
            let doc_id = staged.doc_id_at(record_index).map_err(|err| {
                anyhow::anyhow!("read semantic document id during reconciliation: {err}")
            })?;
            if !current_doc_ids.contains(doc_id) {
                stale_doc_ids.insert(doc_id.to_owned());
            }
        }

        let removed_stale_records = if stale_doc_ids.is_empty() {
            0
        } else {
            let stale_doc_id_refs = stale_doc_ids.iter().map(String::as_str).collect::<Vec<_>>();
            staged
                .soft_delete_batch(&stale_doc_id_refs)
                .map_err(|err| anyhow::anyhow!("remove stale semantic documents: {err}"))?
        };

        let embedded_docs = embedded_messages.len();
        if embedded_docs > 0 {
            let replacement_entries = embedded_messages
                .into_iter()
                .map(|embedded| (semantic_doc_id_for_embedded(&embedded), embedded.embedding))
                .collect::<Vec<_>>();
            staged
                .append_batch(&replacement_entries)
                .map_err(|err| anyhow::anyhow!("append replacement semantic vectors: {err}"))?;
        }
        if staged.wal_record_count() > 0 {
            staged
                .compact()
                .map_err(|err| anyhow::anyhow!("compact replacement semantic vectors: {err}"))?;
        } else if staged.tombstone_count() > 0 {
            staged
                .vacuum()
                .map_err(|err| anyhow::anyhow!("vacuum stale semantic vectors: {err}"))?;
        }
        if staged.wal_record_count() > 0 {
            bail!("reconciled semantic staging index unexpectedly contains WAL records");
        }
        if staged.tombstone_count() > 0 {
            bail!("reconciled semantic staging index unexpectedly contains tombstones");
        }
        if staged.record_count() != current_doc_ids.len() {
            bail!(
                "reconciled semantic staging count mismatch: expected {}, observed {}",
                current_doc_ids.len(),
                staged.record_count()
            );
        }
        let mut staged_doc_ids = HashSet::with_capacity(staged.record_count());
        for record_index in 0..staged.record_count() {
            let doc_id = staged.doc_id_at(record_index).map_err(|err| {
                anyhow::anyhow!("validate reconciled semantic document id: {err}")
            })?;
            if !staged_doc_ids.insert(doc_id.to_owned()) {
                bail!("reconciled semantic staging index contains duplicate document {doc_id}");
            }
            if staged
                .vector_at_f32(record_index)
                .map_err(|err| anyhow::anyhow!("validate reconciled semantic vector: {err}"))?
                .iter()
                .any(|value| !value.is_finite())
            {
                bail!("reconciled semantic staging index contains a non-finite vector");
            }
        }
        if !staged_doc_ids.eq(current_doc_ids) {
            bail!("reconciled semantic staging identities do not match the canonical DB");
        }
        drop(staged);

        if live_wal_snapshot_path.exists() {
            // Publication atomically replaces only the main FSVI. Probe the
            // exact old live WAL against the completed candidate before that
            // swap; it must be rejected as stale rather than replayed.
            fs::copy(&live_wal_snapshot_path, fsvi_wal_path_for(&staging_path)).with_context(
                || {
                    format!(
                        "attach old live WAL to completed semantic candidate {}",
                        staging_path.display()
                    )
                },
            )?;
            let generation_probe = FsVectorIndex::open(&staging_path).map_err(|err| {
                anyhow::anyhow!("validate reconciled semantic WAL generation: {err}")
            })?;
            if generation_probe.wal_record_count() > 0 {
                bail!("reconciled semantic candidate would accept the pre-publication live WAL");
            }
            drop(generation_probe);
        }

        publish_reconciled_semantic_index(&staging_path, &index_path)?;
        let published = FsVectorIndex::open(&index_path)
            .map_err(|err| anyhow::anyhow!("open reconciled semantic index: {err}"))?;
        if published.wal_record_count() > 0 {
            bail!("published reconciled semantic index accepted a stale live WAL");
        }
        tracing::info!(
            retained_docs = current_doc_ids.len().saturating_sub(embedded_docs),
            initial_tombstones,
            removed_stale_records,
            embedded_docs,
            published_docs = published.record_count(),
            "published reconciled semantic index"
        );
        Ok(published)
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
        self.run_backfill_batch_with_sink(
            messages,
            data_dir,
            manifest,
            plan,
            None,
            &SemanticProgressSink::disabled(),
        )
    }

    /// Variant of [`run_backfill_batch`] that emits semantic progress
    /// events to the given JSONL sink and persists `last_message_id`
    /// into the resumable checkpoint when supplied. The sink is silent
    /// unless `CASS_SEMANTIC_PROGRESS_JSONL` is set, so this path is
    /// safe to take in production.
    pub fn run_backfill_batch_with_sink(
        &self,
        messages: &[EmbeddingInput],
        data_dir: &Path,
        manifest: &mut SemanticManifest,
        plan: SemanticBackfillBatchPlan,
        last_message_id: Option<i64>,
        sink: &SemanticProgressSink,
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

        let embeddings = self.embed_messages_with_sink(messages, sink)?;
        let embedded_docs = u64::try_from(embeddings.len()).unwrap_or(u64::MAX);
        if sink.is_active() {
            sink.emit(
                SemanticProgressEvent::StagingWriteStart,
                SemanticProgressFields {
                    batch_rows: Some(embedded_docs),
                    note: Some(staging_path.display().to_string()),
                    ..Default::default()
                },
            );
        }
        let mut staged_index = self.write_backfill_staging_index(
            embeddings,
            &staging_path,
            prior_checkpoint.is_some(),
        )?;
        if sink.is_active() {
            sink.emit(
                SemanticProgressEvent::StagingWriteDone,
                SemanticProgressFields {
                    batch_rows: Some(embedded_docs),
                    note: Some(staging_path.display().to_string()),
                    ..Default::default()
                },
            );
        }
        let counted_conversations_processed =
            prior_conversations.saturating_add(plan.conversations_in_batch);
        let conversations_processed = if plan.cursor_exhausted {
            plan.total_conversations
        } else {
            counted_conversations_processed.min(plan.total_conversations)
        };
        let complete = plan.cursor_exhausted;

        manifest.refresh_backlog(plan.total_conversations, &plan.db_fingerprint);

        if complete {
            let db_fingerprint = plan.db_fingerprint.clone();
            if staged_index.wal_record_count() > 0 {
                staged_index.compact().map_err(|err| {
                    anyhow::anyhow!("compact staged semantic index failed: {err}")
                })?;
            }
            drop(staged_index);
            if sink.is_active() {
                sink.emit(
                    SemanticProgressEvent::PublishStart,
                    SemanticProgressFields {
                        rows_processed: Some(conversations_processed),
                        rows_total: Some(plan.total_conversations),
                        last_conversation_id: Some(plan.last_offset),
                        last_message_id,
                        note: Some(final_path.display().to_string()),
                        ..Default::default()
                    },
                );
            }
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
            if sink.is_active() {
                sink.emit(
                    SemanticProgressEvent::PublishDone,
                    SemanticProgressFields {
                        rows_processed: Some(conversations_processed),
                        rows_total: Some(plan.total_conversations),
                        last_conversation_id: Some(plan.last_offset),
                        last_message_id,
                        note: Some(final_path.display().to_string()),
                        ..Default::default()
                    },
                );
            }
        } else {
            let docs_embedded_on_disk =
                u64::try_from(staged_index.record_count()).unwrap_or(u64::MAX);
            let checkpoint_docs = prior_docs
                .saturating_add(embedded_docs)
                .max(docs_embedded_on_disk);
            if sink.is_active() {
                sink.emit(
                    SemanticProgressEvent::CheckpointSaveStart,
                    SemanticProgressFields {
                        rows_processed: Some(conversations_processed),
                        rows_total: Some(plan.total_conversations),
                        last_conversation_id: Some(plan.last_offset),
                        last_message_id,
                        ..Default::default()
                    },
                );
            }
            // Preserve any existing `last_message_id` cursor when the
            // caller did not supply a fresher one — see sub-fix 2 for
            // why durable message-PK resume matters.
            let prior_last_message_id = prior_checkpoint
                .as_ref()
                .and_then(|checkpoint| checkpoint.last_message_id);
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
                last_message_id: last_message_id.or(prior_last_message_id),
                cursor_exhausted: plan.cursor_exhausted,
            });
            manifest.save(data_dir)?;
            if sink.is_active() {
                sink.emit(
                    SemanticProgressEvent::CheckpointSaveDone,
                    SemanticProgressFields {
                        rows_processed: Some(conversations_processed),
                        rows_total: Some(plan.total_conversations),
                        last_conversation_id: Some(plan.last_offset),
                        last_message_id: last_message_id.or(prior_last_message_id),
                        ..Default::default()
                    },
                );
            }
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
        self.run_backfill_from_storage_with_sink(
            storage,
            data_dir,
            manifest,
            plan,
            &SemanticProgressSink::disabled(),
        )
    }

    /// Variant of [`run_backfill_from_storage`] that emits semantic
    /// progress events to a JSONL sink and persists `last_message_id`
    /// in the resumable checkpoint. The sink is silent unless
    /// `CASS_SEMANTIC_PROGRESS_JSONL` is set.
    pub fn run_backfill_from_storage_with_sink(
        &self,
        storage: &FrankenStorage,
        data_dir: &Path,
        manifest: &mut SemanticManifest,
        plan: SemanticBackfillStoragePlan,
        sink: &SemanticProgressSink,
    ) -> Result<SemanticBackfillBatchOutcome> {
        self.run_backfill_from_storage_with_caps_and_sink(
            storage,
            data_dir,
            manifest,
            plan,
            SemanticCheckpointCaps::unlimited(),
            sink,
        )
    }

    /// Variant of [`run_backfill_from_storage_with_sink`] for CLI backfill
    /// runs. It applies operator checkpoint caps from
    /// `CASS_SEMANTIC_MAX_MESSAGES_PER_CHECKPOINT` and
    /// `CASS_SEMANTIC_MAX_BYTES_PER_CHECKPOINT` while keeping each selected
    /// conversation whole, so message-cursor resume cannot strand the tail of
    /// a partially selected conversation.
    pub fn run_capped_backfill_from_storage_with_sink(
        &self,
        storage: &FrankenStorage,
        data_dir: &Path,
        manifest: &mut SemanticManifest,
        plan: SemanticBackfillStoragePlan,
        sink: &SemanticProgressSink,
    ) -> Result<SemanticBackfillBatchOutcome> {
        self.run_backfill_from_storage_with_caps_and_sink(
            storage,
            data_dir,
            manifest,
            plan,
            SemanticCheckpointCaps::from_env(),
            sink,
        )
    }

    fn run_backfill_from_storage_with_caps_and_sink(
        &self,
        storage: &FrankenStorage,
        data_dir: &Path,
        manifest: &mut SemanticManifest,
        plan: SemanticBackfillStoragePlan,
        caps: SemanticCheckpointCaps,
        sink: &SemanticProgressSink,
    ) -> Result<SemanticBackfillBatchOutcome> {
        let prior_checkpoint = manifest.checkpoint.as_ref().filter(|checkpoint| {
            checkpoint.tier == plan.tier
                && checkpoint.embedder_id == self.embedder_id()
                && checkpoint.is_valid(&plan.db_fingerprint)
        });
        let after_conversation_id = prior_checkpoint.map_or(0, |checkpoint| checkpoint.last_offset);
        let prior_last_message_id =
            prior_checkpoint.and_then(|checkpoint| checkpoint.last_message_id);
        let cached_total_conversations = cached_semantic_total_conversations(
            manifest,
            plan.tier,
            self.embedder_id(),
            &plan.db_fingerprint,
        );

        if sink.is_active() {
            sink.emit(
                SemanticProgressEvent::SelectionStart,
                SemanticProgressFields {
                    last_conversation_id: Some(after_conversation_id),
                    last_message_id: prior_last_message_id,
                    rows_total: Some(saturating_u64_from_usize(plan.max_conversations)),
                    note: Some(plan.tier.as_str().to_string()),
                    ..Default::default()
                },
            );
            sink.emit(
                SemanticProgressEvent::SelectionCountStart,
                SemanticProgressFields {
                    last_conversation_id: Some(after_conversation_id),
                    last_message_id: prior_last_message_id,
                    note: Some(
                        cached_total_conversations
                            .map_or("database", |(_, source)| source)
                            .to_string(),
                    ),
                    ..Default::default()
                },
            );
        }

        let total_conversations = match cached_total_conversations {
            Some((total, _)) => total,
            None => match total_semantic_conversations(storage) {
                Ok(total) => total,
                Err(err) => {
                    if sink.is_active() {
                        sink.emit(
                            SemanticProgressEvent::Error,
                            SemanticProgressFields {
                                error: Some(format!("selection count: {err}")),
                                last_conversation_id: Some(after_conversation_id),
                                last_message_id: prior_last_message_id,
                                ..Default::default()
                            },
                        );
                    }
                    return Err(err);
                }
            },
        };

        if sink.is_active() {
            sink.emit(
                SemanticProgressEvent::SelectionCountDone,
                SemanticProgressFields {
                    rows_processed: Some(total_conversations),
                    rows_total: Some(total_conversations),
                    last_conversation_id: Some(after_conversation_id),
                    last_message_id: prior_last_message_id,
                    note: Some(
                        cached_total_conversations
                            .map_or("database", |(_, source)| source)
                            .to_string(),
                    ),
                    ..Default::default()
                },
            );
        }

        let batch = match fetch_canonical_embedding_batch_inner_with_caps_and_total(
            storage,
            after_conversation_id,
            plan.max_conversations,
            prior_last_message_id,
            caps,
            total_conversations,
            Some(sink),
        ) {
            Ok(batch) => batch,
            Err(err) => {
                if sink.is_active() {
                    sink.emit(
                        SemanticProgressEvent::Error,
                        SemanticProgressFields {
                            error: Some(format!("selection: {err}")),
                            last_conversation_id: Some(after_conversation_id),
                            last_message_id: prior_last_message_id,
                            ..Default::default()
                        },
                    );
                }
                return Err(err);
            }
        };

        if sink.is_active() {
            sink.emit(
                SemanticProgressEvent::SelectionDone,
                SemanticProgressFields {
                    last_conversation_id: Some(batch.last_conversation_id),
                    last_message_id: prior_last_message_id,
                    conversations_in_batch: Some(batch.conversations_in_batch),
                    rows_total: Some(batch.total_conversations),
                    ..Default::default()
                },
            );
            // PacketReplay {start,progress,done} bracket the
            // envelope/messages/packet build done by
            // `fetch_canonical_embedding_batch`. We can't easily plumb
            // a callback inside that helper without refactoring it, so
            // the start/done bracket here straddles the work that
            // already happened (replay always finishes before we see
            // the result). A future refactor — flagged in the
            // SQL-shape follow-up — can move the packet-replay work
            // into a streaming iterator and emit `progress` ticks
            // per conversation. For now, the bracket still gives
            // operators a clear "we got past replay" signal.
            sink.emit(
                SemanticProgressEvent::PacketReplayStart,
                SemanticProgressFields {
                    conversations_in_batch: Some(batch.conversations_in_batch),
                    ..Default::default()
                },
            );
            sink.emit(
                SemanticProgressEvent::PacketReplayProgress,
                SemanticProgressFields {
                    conversations_in_batch: Some(batch.conversations_in_batch),
                    rows_processed: Some(saturating_u64_from_usize(batch.inputs.len())),
                    bytes: Some(
                        batch
                            .inputs
                            .iter()
                            .map(|i| saturating_u64_from_usize(i.content.len()))
                            .sum(),
                    ),
                    ..Default::default()
                },
            );
            sink.emit(
                SemanticProgressEvent::PacketReplayDone,
                SemanticProgressFields {
                    conversations_in_batch: Some(batch.conversations_in_batch),
                    rows_processed: Some(saturating_u64_from_usize(batch.inputs.len())),
                    ..Default::default()
                },
            );
        }

        // Compute the freshest `last_message_id` from the inputs we are
        // about to embed. EmbeddingInput.message_id is u64 (canonical
        // message PK); we coerce to i64 since the manifest stores i64.
        let batch_last_message_id = batch
            .inputs
            .iter()
            .map(|input| i64::try_from(input.message_id).unwrap_or(i64::MAX))
            .max();
        let next_last_message_id = match (prior_last_message_id, batch_last_message_id) {
            (Some(prior), Some(batch_max)) => Some(prior.max(batch_max)),
            (Some(prior), None) => Some(prior),
            (None, Some(batch_max)) => Some(batch_max),
            (None, None) => None,
        };

        let outcome = self.run_backfill_batch_with_sink(
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
                cursor_exhausted: batch.cursor_exhausted,
            },
            next_last_message_id,
            sink,
        );

        match &outcome {
            Ok(o) => {
                if sink.is_active() {
                    sink.emit(
                        SemanticProgressEvent::Complete,
                        SemanticProgressFields {
                            rows_processed: Some(o.conversations_processed),
                            rows_total: Some(o.total_conversations),
                            last_conversation_id: Some(o.last_offset),
                            last_message_id: next_last_message_id,
                            ..Default::default()
                        },
                    );
                }
            }
            Err(err) => {
                if sink.is_active() {
                    sink.emit(
                        SemanticProgressEvent::Error,
                        SemanticProgressFields {
                            error: Some(err.to_string()),
                            last_conversation_id: Some(batch.last_conversation_id),
                            last_message_id: next_last_message_id,
                            ..Default::default()
                        },
                    );
                }
            }
        }

        outcome
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
    use std::path::Path;
    use tempfile::tempdir;

    /// cass #309: length-aware embed batching must cap both row count and
    /// `row_count × max_canonical_len` per batch, never drop or reorder rows,
    /// and keep a single over-budget message as its own one-row batch.
    #[test]
    fn length_aware_batches_bounds_count_and_chars() {
        fn rows<'a>(inputs: &'a [EmbeddingInput], lens: &[usize]) -> Vec<Prepared<'a>> {
            lens.iter()
                .enumerate()
                .map(|(i, &len)| Prepared {
                    msg: &inputs[i],
                    canonical: "a".repeat(len),
                    hash: [0u8; 32],
                })
                .collect()
        }
        let inputs: Vec<EmbeddingInput> =
            (0..6).map(|i| EmbeddingInput::new(i as u64, "")).collect();

        // 1) Count cap only (budget disabled): fixed chunks of max_count.
        let r = rows(&inputs, &[10, 10, 10, 10, 10]);
        assert_eq!(
            length_aware_batches(&r, 2, 0)
                .iter()
                .map(|b| b.len())
                .collect::<Vec<_>>(),
            vec![2, 2, 1]
        );

        // 2) Char budget caps batches that hold longer rows (50 each, budget
        //    100 -> at most 2 rows/batch); order is preserved.
        let r = rows(&inputs, &[50, 50, 50, 50, 50]);
        let b = length_aware_batches(&r, 128, 100);
        assert!(b.iter().all(|s| s.len() <= 2));
        let flat: Vec<u64> = b
            .iter()
            .flat_map(|s| s.iter().map(|p| p.msg.message_id))
            .collect();
        assert_eq!(flat, vec![0, 1, 2, 3, 4]);

        // 3) A single over-budget row forms its own batch; nothing is dropped.
        let r = rows(&inputs, &[10, 500, 10]);
        let b = length_aware_batches(&r, 128, 100);
        assert_eq!(b.iter().map(|s| s.len()).sum::<usize>(), 3);
        assert!(
            b.iter()
                .any(|s| s.len() == 1 && s[0].canonical.len() == 500)
        );

        // 4) Empty input -> no batches.
        assert!(length_aware_batches(&[], 8, 100).is_empty());
    }

    #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
    struct ComparableSemanticInput {
        message_id: u64,
        created_at_ms: i64,
        agent_id: u32,
        workspace_id: u32,
        source_id: u32,
        role: u8,
        content: String,
    }

    fn comparable_semantic_inputs(mut inputs: Vec<EmbeddingInput>) -> Vec<ComparableSemanticInput> {
        let mut comparable: Vec<ComparableSemanticInput> = inputs
            .drain(..)
            .map(|input| ComparableSemanticInput {
                message_id: input.message_id,
                created_at_ms: input.created_at_ms,
                agent_id: input.agent_id,
                workspace_id: input.workspace_id,
                source_id: input.source_id,
                role: input.role,
                content: input.content,
            })
            .collect();
        comparable.sort();
        comparable
    }

    fn test_conversation(external_id: &str, body: &str) -> Conversation {
        test_conversation_fixture(
            external_id,
            vec![Message {
                id: None,
                idx: 0,
                role: MessageRole::User,
                author: None,
                created_at: Some(1_700_000_000_500),
                content: body.to_string(),
                extra_json: json!({}),
                snippets: Vec::new(),
            }],
            "local",
            None,
        )
    }

    fn test_conversation_with_messages(external_id: &str, messages: Vec<Message>) -> Conversation {
        test_conversation_fixture(external_id, messages, "remote-laptop", Some("builder-host"))
    }

    fn test_conversation_fixture(
        external_id: &str,
        messages: Vec<Message>,
        source_id: &str,
        origin_host: Option<&str>,
    ) -> Conversation {
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
            messages,
            source_id: source_id.to_string(),
            origin_host: origin_host.map(str::to_string),
        }
    }

    fn default_scheduler_signals() -> SemanticBackfillSchedulerSignals {
        SemanticBackfillSchedulerSignals {
            foreground_pressure: false,
            lexical_repair_active: false,
            force: false,
            operator_disabled: false,
        }
    }

    struct EnvVarGuard {
        key: &'static str,
        prior: Option<String>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let prior = std::env::var(key).ok();
            // SAFETY: focused tests temporarily mutate process env and restore on drop.
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, prior }
        }

        fn remove(key: &'static str) -> Self {
            let prior = std::env::var(key).ok();
            // SAFETY: focused tests temporarily mutate process env and restore on drop.
            unsafe {
                std::env::remove_var(key);
            }
            Self { key, prior }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            // SAFETY: restores the process env value captured by this test guard.
            unsafe {
                match self.prior.as_deref() {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    #[test]
    fn semantic_backfill_scheduler_runs_and_scales_batch_under_idle_budget() {
        let policy = SemanticPolicy::compiled_defaults();
        let decision = semantic_backfill_scheduler_decision_for_capacity(
            &policy,
            64,
            &default_scheduler_signals(),
            80,
        );

        assert!(decision.should_run());
        assert_eq!(decision.state, SemanticBackfillSchedulerState::Running);
        assert_eq!(
            decision.reason,
            SemanticBackfillSchedulerReason::IdleBudgetAvailable
        );
        assert_eq!(decision.scheduled_batch_conversations, 51);
        assert_eq!(decision.current_capacity_pct, 80);
        assert_eq!(decision.next_eligible_after_ms, 0);
    }

    #[test]
    fn semantic_backfill_scheduler_reason_next_steps_are_stable() {
        for (reason, expected) in [
            (
                SemanticBackfillSchedulerReason::IdleBudgetAvailable,
                "background semantic backfill is within idle budgets",
            ),
            (
                SemanticBackfillSchedulerReason::OperatorDisabled,
                "background semantic backfill is disabled by CASS_SEMANTIC_BACKFILL_DISABLE",
            ),
            (
                SemanticBackfillSchedulerReason::PolicyDisabled,
                "semantic policy disables background semantic backfill",
            ),
            (
                SemanticBackfillSchedulerReason::ForegroundPressure,
                "foreground pressure is present; retry after the idle delay",
            ),
            (
                SemanticBackfillSchedulerReason::LexicalRepairActive,
                "lexical repair is active; semantic backfill is yielding",
            ),
            (
                SemanticBackfillSchedulerReason::CapacityBelowFloor,
                "machine responsiveness capacity is below the semantic backfill floor",
            ),
            (
                SemanticBackfillSchedulerReason::ThreadBudgetZero,
                "semantic backfill thread budget is zero",
            ),
            (
                SemanticBackfillSchedulerReason::BatchBudgetZero,
                "semantic backfill batch budget is zero",
            ),
        ] {
            assert_eq!(reason.next_step(), expected, "{reason:?}");
        }
    }

    #[test]
    fn semantic_backfill_scheduler_yields_to_foreground_and_lexical_pressure() {
        let policy = SemanticPolicy::compiled_defaults();
        let foreground = SemanticBackfillSchedulerSignals {
            foreground_pressure: true,
            ..default_scheduler_signals()
        };
        let foreground_decision =
            semantic_backfill_scheduler_decision_for_capacity(&policy, 64, &foreground, 100);
        assert!(!foreground_decision.should_run());
        assert_eq!(
            foreground_decision.state,
            SemanticBackfillSchedulerState::Paused
        );
        assert_eq!(
            foreground_decision.reason,
            SemanticBackfillSchedulerReason::ForegroundPressure
        );
        assert_eq!(
            foreground_decision.next_eligible_after_ms,
            policy.idle_delay_seconds * 1000
        );

        let lexical_repair = SemanticBackfillSchedulerSignals {
            lexical_repair_active: true,
            ..default_scheduler_signals()
        };
        let lexical_decision =
            semantic_backfill_scheduler_decision_for_capacity(&policy, 64, &lexical_repair, 100);
        assert!(!lexical_decision.should_run());
        assert_eq!(
            lexical_decision.state,
            SemanticBackfillSchedulerState::Paused
        );
        assert_eq!(
            lexical_decision.reason,
            SemanticBackfillSchedulerReason::LexicalRepairActive
        );
    }

    #[test]
    fn semantic_backfill_scheduler_honors_policy_disable_and_force_override() {
        let mut policy = SemanticPolicy::compiled_defaults();
        policy.mode = crate::search::policy::SemanticMode::LexicalOnly;

        let disabled = semantic_backfill_scheduler_decision_for_capacity(
            &policy,
            64,
            &default_scheduler_signals(),
            100,
        );
        assert!(!disabled.should_run());
        assert_eq!(disabled.state, SemanticBackfillSchedulerState::Disabled);
        assert_eq!(
            disabled.reason,
            SemanticBackfillSchedulerReason::PolicyDisabled
        );

        let forced = SemanticBackfillSchedulerSignals {
            force: true,
            ..default_scheduler_signals()
        };
        let forced_decision =
            semantic_backfill_scheduler_decision_for_capacity(&policy, 64, &forced, 100);
        assert!(forced_decision.should_run());
        assert_eq!(
            forced_decision.reason,
            SemanticBackfillSchedulerReason::IdleBudgetAvailable
        );
        assert!(forced_decision.forced);
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
    fn embedding_validation_rejects_non_finite_and_wrong_dimension() -> Result<()> {
        for (vector, expected_message) in [
            (vec![f32::NAN; 384], "non-finite"),
            (vec![f32::INFINITY; 384], "non-finite"),
            (vec![0.0; 383], "dimension mismatch"),
        ] {
            let Err(error) = validate_embedding_vector(&vector, 384, 7) else {
                bail!("invalid vector unexpectedly passed validation");
            };
            if !error.to_string().contains(expected_message) {
                bail!("unexpected validation error: {error:#}");
            }
        }
        Ok(())
    }

    #[test]
    fn append_rejects_legacy_revision_and_invalid_new_vectors_without_mutation() -> Result<()> {
        let indexer = SemanticIndexer::new("hash", None)?;
        let tmp = tempdir()?;
        let index_path = vector_index_path(tmp.path(), indexer.embedder_id());
        let Some(index_parent) = index_path.parent() else {
            bail!("vector index path has no parent");
        };
        fs::create_dir_all(index_parent)?;

        let mut base_vectors =
            indexer.embed_messages(&[EmbeddingInput::new(1, "legacy vector")])?;
        let Some(base) = base_vectors.pop() else {
            bail!("embedding produced no vector");
        };
        let mut writer = FsVectorIndex::create_with_revision(
            &index_path,
            indexer.embedder_id(),
            "1.0",
            indexer.embedder_dimension(),
            FsQuantization::F16,
        )?;
        writer
            .write_record(&semantic_doc_id_for_embedded(&base), &base.embedding)
            .map_err(|error| anyhow::anyhow!(error))?;
        writer.finish().map_err(|error| anyhow::anyhow!(error))?;

        let candidate = indexer.embed_messages(&[EmbeddingInput::new(2, "current vector")])?;
        let Err(error) = indexer.append_to_index(candidate, tmp.path()) else {
            bail!("legacy revision was accepted");
        };
        if !error.to_string().contains("revision 1.0") {
            bail!("unexpected legacy-revision error: {error:#}");
        }
        let unchanged = FsVectorIndex::open(&index_path).map_err(|error| anyhow::anyhow!(error))?;
        if unchanged.record_count() != 1 || unchanged.wal_record_count() != 0 {
            bail!("legacy append mutated the index");
        }
        drop(unchanged);

        let mut current_writer = FsVectorIndex::create_with_revision(
            &index_path,
            indexer.embedder_id(),
            indexer.vector_space_revision()?,
            indexer.embedder_dimension(),
            FsQuantization::F16,
        )?;
        current_writer
            .write_record(&semantic_doc_id_for_embedded(&base), &base.embedding)
            .map_err(|error| anyhow::anyhow!(error))?;
        current_writer
            .finish()
            .map_err(|error| anyhow::anyhow!(error))?;

        let mut non_finite_vectors =
            indexer.embed_messages(&[EmbeddingInput::new(3, "invalid append")])?;
        let Some(mut non_finite) = non_finite_vectors.pop() else {
            bail!("embedding produced no vector");
        };
        non_finite.embedding.fill(f32::NAN);
        let Err(error) = indexer.append_to_index([non_finite], tmp.path()) else {
            bail!("non-finite append was accepted");
        };
        if !error.to_string().contains("non-finite") {
            bail!("unexpected non-finite error: {error:#}");
        }

        let mut wrong_dimension_vectors =
            indexer.embed_messages(&[EmbeddingInput::new(4, "wrong dimension")])?;
        let Some(mut wrong_dimension) = wrong_dimension_vectors.pop() else {
            bail!("embedding produced no vector");
        };
        wrong_dimension.embedding.pop();
        let Err(error) = indexer.append_to_index([wrong_dimension], tmp.path()) else {
            bail!("wrong-dimension append was accepted");
        };
        if !error.to_string().contains("dimension mismatch") {
            bail!("unexpected dimension error: {error:#}");
        }

        let unchanged = FsVectorIndex::open(&index_path).map_err(|error| anyhow::anyhow!(error))?;
        if unchanged.record_count() != 1 || unchanged.wal_record_count() != 0 {
            bail!("invalid append mutated the current index");
        }
        Ok(())
    }

    #[test]
    fn issue_342_direct_semantic_progress_callbacks_reach_exact_totals() {
        let indexer = SemanticIndexer::new("hash", None).unwrap();
        let messages = vec![
            EmbeddingInput::new(1, "Hello world"),
            EmbeddingInput::new(2, "Goodbye world"),
        ];
        let mut embedding_progress = Vec::new();
        let embeddings = indexer
            .embed_messages_with_progress(&messages, |current, total| {
                embedding_progress.push((current, total));
            })
            .unwrap();
        assert_eq!(embedding_progress.last(), Some(&(2, 2)));

        let tmp = tempdir().unwrap();
        let mut vector_progress = Vec::new();
        let index = indexer
            .build_and_save_index_with_progress(embeddings, tmp.path(), |current| {
                vector_progress.push(current);
            })
            .unwrap();
        assert_eq!(vector_progress, vec![1, 2]);
        assert_eq!(index.record_count(), 2);
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

    #[test]
    fn semantic_reconciliation_rejects_invalid_replacement_without_mutating_live_fsvi_or_wal() {
        let indexer = SemanticIndexer::new("hash", None).unwrap();
        let tmp = tempdir().unwrap();
        let base_inputs: Vec<_> = (1..=20)
            .map(|message_id| {
                EmbeddingInput::new(
                    message_id,
                    format!("semantic reconciliation base message {message_id}"),
                )
            })
            .collect();
        let base_embeddings = indexer.embed_messages(&base_inputs).unwrap();
        drop(
            indexer
                .build_and_save_index(base_embeddings, tmp.path())
                .unwrap(),
        );

        let wal_input = EmbeddingInput::new(21, "semantic reconciliation pending WAL message");
        let wal_embeddings = indexer
            .embed_messages(std::slice::from_ref(&wal_input))
            .unwrap();
        assert_eq!(
            indexer.append_to_index(wal_embeddings, tmp.path()).unwrap(),
            1
        );

        let index_path = vector_index_path(tmp.path(), indexer.embedder_id());
        let wal_path = fsvi_wal_path_for(&index_path);
        let live = FsVectorIndex::open(&index_path).unwrap();
        assert_eq!(live.record_count(), 20);
        assert_eq!(
            live.wal_record_count(),
            1,
            "the fixture must exercise a real uncompacted WAL"
        );
        drop(live);
        let main_before = fs::read(&index_path).unwrap();
        let wal_before = fs::read(&wal_path).unwrap();

        let replacement_input =
            EmbeddingInput::new(22, "semantic reconciliation invalid replacement");
        let mut invalid_replacement = indexer
            .embed_messages(std::slice::from_ref(&replacement_input))
            .unwrap()
            .pop()
            .unwrap();
        invalid_replacement.embedding.pop();
        let current_doc_ids: HashSet<_> = base_inputs
            .iter()
            .chain([&wal_input, &replacement_input])
            .filter_map(semantic_doc_id_for_input)
            .collect();

        let err = indexer
            .reconcile_index_with_canonical_documents(
                vec![invalid_replacement],
                tmp.path(),
                TierKind::Fast,
                "content-v1:22:22:22",
                &current_doc_ids,
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("embedding dimension mismatch"),
            "unexpected reconciliation error: {err:#}"
        );
        assert_eq!(
            fs::read(&index_path).unwrap(),
            main_before,
            "invalid replacements must not rewrite the published FSVI"
        );
        assert_eq!(
            fs::read(&wal_path).unwrap(),
            wal_before,
            "invalid replacements must not compact or rewrite the published WAL"
        );
        let live_after = FsVectorIndex::open(&index_path).unwrap();
        assert_eq!(live_after.record_count(), 20);
        assert_eq!(live_after.wal_record_count(), 1);
    }

    #[test]
    fn semantic_reconciliation_publishes_wal_entries_without_replaying_stale_sidecar() {
        let indexer = SemanticIndexer::new("hash", None).unwrap();
        let tmp = tempdir().unwrap();
        let base_inputs: Vec<_> = (1..=20)
            .map(|message_id| {
                EmbeddingInput::new(
                    message_id,
                    format!("semantic reconciliation base message {message_id}"),
                )
            })
            .collect();
        let base_embeddings = indexer.embed_messages(&base_inputs).unwrap();
        drop(
            indexer
                .build_and_save_index(base_embeddings, tmp.path())
                .unwrap(),
        );

        let wal_input = EmbeddingInput::new(21, "semantic reconciliation pending WAL message");
        let wal_embeddings = indexer
            .embed_messages(std::slice::from_ref(&wal_input))
            .unwrap();
        assert_eq!(
            indexer
                .append_to_index(wal_embeddings.clone(), tmp.path())
                .unwrap(),
            1
        );

        let index_path = vector_index_path(tmp.path(), indexer.embedder_id());
        let wal_path = fsvi_wal_path_for(&index_path);
        let before = FsVectorIndex::open(&index_path).unwrap();
        assert_eq!(before.metadata().compaction_gen, 1);
        assert_eq!(before.wal_record_count(), 1);
        assert!(wal_path.exists());
        drop(before);

        let current_doc_ids: HashSet<_> = base_inputs
            .iter()
            .chain([&wal_input])
            .filter_map(semantic_doc_id_for_input)
            .collect();
        let published = indexer
            .reconcile_index_with_canonical_documents(
                wal_embeddings,
                tmp.path(),
                TierKind::Fast,
                "content-v1:21:21:21",
                &current_doc_ids,
            )
            .unwrap();

        assert_eq!(published.record_count(), 21);
        assert_eq!(published.wal_record_count(), 0);
        assert_eq!(published.tombstone_count(), 0);
        assert!(
            !wal_path.exists(),
            "opening the published generation must discard the old live WAL"
        );
        let published_doc_ids: HashSet<_> = (0..published.record_count())
            .map(|record_index| published.doc_id_at(record_index).unwrap().to_owned())
            .collect();
        assert_eq!(published_doc_ids, current_doc_ids);
    }

    #[test]
    fn semantic_reconciliation_replaces_duplicates_and_tombstones_in_private_candidate() {
        let indexer = SemanticIndexer::new("hash", None).unwrap();
        let tmp = tempdir().unwrap();
        let inputs = [
            EmbeddingInput::new(1, "semantic reconciliation duplicated message"),
            EmbeddingInput::new(2, "semantic reconciliation tombstoned message"),
        ];
        let embeddings = indexer.embed_messages(&inputs).unwrap();
        let doc_ids = inputs
            .iter()
            .filter_map(semantic_doc_id_for_input)
            .collect::<Vec<_>>();
        let index_path = vector_index_path(tmp.path(), indexer.embedder_id());
        fs::create_dir_all(index_path.parent().unwrap()).unwrap();
        let mut writer = FsVectorIndex::create_with_revision(
            &index_path,
            indexer.embedder_id(),
            HASH_VECTOR_SPACE_REVISION,
            indexer.embedder_dimension(),
            FsQuantization::F16,
        )
        .unwrap();
        writer
            .write_record(&doc_ids[0], &embeddings[0].embedding)
            .unwrap();
        writer
            .write_record(&doc_ids[0], &embeddings[0].embedding)
            .unwrap();
        writer
            .write_record(&doc_ids[1], &embeddings[1].embedding)
            .unwrap();
        writer.finish().unwrap();
        let mut live = FsVectorIndex::open(&index_path).unwrap();
        assert!(live.soft_delete(&doc_ids[1]).unwrap());
        assert_eq!(live.record_count(), 3);
        assert_eq!(live.tombstone_count(), 1);
        drop(live);

        let current_doc_ids = doc_ids.into_iter().collect::<HashSet<_>>();
        let published = indexer
            .reconcile_index_with_canonical_documents(
                embeddings,
                tmp.path(),
                TierKind::Fast,
                "content-v1:2:2:2",
                &current_doc_ids,
            )
            .unwrap();

        assert_eq!(published.record_count(), 2);
        assert_eq!(published.wal_record_count(), 0);
        assert_eq!(published.tombstone_count(), 0);
        let published_doc_ids = (0..published.record_count())
            .map(|record_index| published.doc_id_at(record_index).unwrap().to_owned())
            .collect::<HashSet<_>>();
        assert_eq!(published_doc_ids, current_doc_ids);
    }

    #[test]
    fn sharded_index_build_writes_sidecar_without_runtime_publish() {
        let indexer = SemanticIndexer::new("hash", None).unwrap();
        let messages: Vec<_> = (0..5)
            .map(|idx| EmbeddingInput::new(idx, format!("semantic shard message {idx}")))
            .collect();
        let embeddings = indexer.embed_messages(&messages).unwrap();
        let tmp = tempdir().unwrap();

        let outcome = indexer
            .build_and_save_index_shards(
                embeddings,
                tmp.path(),
                SemanticShardBuildPlan {
                    tier: TierKind::Fast,
                    db_fingerprint: "db-fp-sharded-build".to_string(),
                    model_revision: "hash".to_string(),
                    total_conversations: 5,
                    max_records_per_shard: 2,
                    build_ann: false,
                },
            )
            .unwrap();

        assert_eq!(outcome.shard_count, 3);
        assert_eq!(outcome.doc_count, 5);
        assert_eq!(outcome.total_conversations, 5);
        assert!(outcome.complete);
        assert_eq!(outcome.index_paths.len(), 3);
        for path in &outcome.index_paths {
            let shard = FsVectorIndex::open(path).unwrap();
            assert_eq!(shard.embedder_id(), indexer.embedder_id());
            assert!(shard.record_count() > 0);
        }

        let shards = SemanticShardManifest::load(tmp.path()).unwrap().unwrap();
        let summary = shards.summary(TierKind::Fast, indexer.embedder_id(), "db-fp-sharded-build");
        assert!(summary.complete);
        assert_eq!(summary.ready_shards, 3);
        assert_eq!(summary.ann_ready_shards, 0);
        assert_eq!(summary.doc_count, 5);
        assert_eq!(summary.total_conversations, 5);

        assert!(
            SemanticManifest::load(tmp.path()).unwrap().is_none(),
            "sidecar shards must not publish the main runtime manifest"
        );
        assert!(!vector_index_path(tmp.path(), indexer.embedder_id()).exists());
    }

    #[test]
    fn sharded_index_build_rejects_zero_sized_shards() {
        let indexer = SemanticIndexer::new("hash", None).unwrap();
        let err = indexer
            .build_and_save_index_shards(
                std::iter::empty(),
                tempdir().unwrap().path(),
                SemanticShardBuildPlan {
                    tier: TierKind::Fast,
                    db_fingerprint: "db-fp-sharded-build".to_string(),
                    model_revision: "hash".to_string(),
                    total_conversations: 0,
                    max_records_per_shard: 0,
                    build_ann: false,
                },
            )
            .unwrap_err();

        assert!(err.to_string().contains("max_records_per_shard > 0"));
    }

    #[test]
    fn sharded_ann_build_records_per_shard_accelerators() {
        let indexer = SemanticIndexer::new("hash", None).unwrap();
        let messages: Vec<_> = (0..8)
            .map(|idx| EmbeddingInput::new(idx, format!("semantic ann shard message {idx}")))
            .collect();
        let embeddings = indexer.embed_messages(&messages).unwrap();
        let tmp = tempdir().unwrap();

        let outcome = indexer
            .build_and_save_index_shards(
                embeddings,
                tmp.path(),
                SemanticShardBuildPlan {
                    tier: TierKind::Fast,
                    db_fingerprint: "db-fp-sharded-ann-build".to_string(),
                    model_revision: "hash".to_string(),
                    total_conversations: 8,
                    max_records_per_shard: 4,
                    build_ann: true,
                },
            )
            .unwrap();

        assert_eq!(outcome.shard_count, 2);
        assert_eq!(outcome.ann_index_paths.len(), 2);
        for path in &outcome.ann_index_paths {
            assert!(path.exists(), "ANN shard missing at {}", path.display());
        }

        let shards = SemanticShardManifest::load(tmp.path()).unwrap().unwrap();
        let summary = shards.summary(
            TierKind::Fast,
            indexer.embedder_id(),
            "db-fp-sharded-ann-build",
        );
        assert!(summary.complete);
        assert_eq!(summary.ann_ready_shards, 2);
        assert!(summary.ann_size_bytes > 0);
        assert!(
            shards
                .shards
                .iter()
                .all(|record| record.ann_index_path.is_some() && record.ann_ready)
        );
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
    fn memoized_serial_prep_matches_stateless_prepare_window() {
        let inputs = vec![
            EmbeddingInput::new(1, "repeat me exactly"),
            EmbeddingInput::new(2, "repeat me exactly"),
            EmbeddingInput::new(3, "unique payload"),
            EmbeddingInput::new(4, ""),
            EmbeddingInput::new(5, "repeat me exactly"),
        ];

        let baseline = prepare_window(&inputs, true);
        let mut cache = ContentAddressedMemoCache::with_capacity(16);
        let memoized = prepare_window_with_memo(&inputs, &mut cache);

        assert_eq!(baseline.len(), memoized.len());
        for (plain, cached) in baseline.iter().zip(memoized.iter()) {
            assert_eq!(plain.msg.message_id, cached.msg.message_id);
            assert_eq!(plain.canonical, cached.canonical);
            assert_eq!(plain.hash, cached.hash);
        }
    }

    #[test]
    fn semantic_prep_memo_key_uses_stable_content_hash_bytes() {
        let key = semantic_prep_memo_key("repeat me exactly");
        let expected = content_hash("repeat me exactly");

        assert_eq!(key.content_hash.as_bytes(), expected.as_slice());
        assert_eq!(key.content_hash.as_bytes().len(), expected.len());
        assert_eq!(key.algorithm, SEMANTIC_PREP_MEMO_ALGORITHM);
        assert_eq!(key.algorithm_version, SEMANTIC_PREP_MEMO_VERSION);
    }

    #[test]
    fn memoized_serial_prep_reuses_duplicate_content_across_windows() {
        let inputs = vec![
            EmbeddingInput::new(1, "repeat me exactly"),
            EmbeddingInput::new(2, "repeat me exactly"),
            EmbeddingInput::new(3, "unique payload"),
            EmbeddingInput::new(4, ""),
            EmbeddingInput::new(5, "repeat me exactly"),
        ];

        let mut cache = ContentAddressedMemoCache::with_capacity(16);
        let prepared = prepare_window_with_memo(&inputs, &mut cache);
        let stats = cache.stats().clone();

        assert_eq!(prepared.len(), 4);
        assert_eq!(stats.hits, 2);
        assert_eq!(stats.misses, 3);
        assert_eq!(stats.inserts, 2);
        assert_eq!(stats.live_entries, 2);
    }

    #[test]
    fn packet_embedding_inputs_reuse_memoized_prep_for_duplicate_content() -> Result<()> {
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
            &test_conversation_with_messages(
                "packet-memo-conv-one",
                vec![
                    Message {
                        id: None,
                        idx: 0,
                        role: MessageRole::User,
                        author: None,
                        created_at: Some(1_700_000_010_100),
                        content: "shared semantic payload".to_string(),
                        extra_json: json!({}),
                        snippets: Vec::new(),
                    },
                    Message {
                        id: None,
                        idx: 1,
                        role: MessageRole::Agent,
                        author: None,
                        created_at: Some(1_700_000_010_200),
                        content: "unique semantic payload one".to_string(),
                        extra_json: json!({}),
                        snippets: Vec::new(),
                    },
                ],
            ),
        )?;
        storage.insert_conversation_tree(
            agent_id,
            None,
            &test_conversation_with_messages(
                "packet-memo-conv-two",
                vec![
                    Message {
                        id: None,
                        idx: 0,
                        role: MessageRole::Tool,
                        author: None,
                        created_at: Some(1_700_000_010_300),
                        content: "shared semantic payload".to_string(),
                        extra_json: json!({}),
                        snippets: Vec::new(),
                    },
                    Message {
                        id: None,
                        idx: 1,
                        role: MessageRole::Agent,
                        author: None,
                        created_at: Some(1_700_000_010_400),
                        content: "unique semantic payload two".to_string(),
                        extra_json: json!({}),
                        snippets: Vec::new(),
                    },
                ],
            ),
        )?;

        let packet_inputs = packet_embedding_inputs_from_storage(&storage)?;
        let mut cache = ContentAddressedMemoCache::with_capacity(16);
        let prepared = prepare_window_with_memo(&packet_inputs, &mut cache);
        let stats = cache.stats().clone();

        assert_eq!(packet_inputs.len(), 4);
        assert_eq!(prepared.len(), 4);
        assert_eq!(
            semantic_prep_memo_key("shared semantic payload")
                .content_hash
                .as_bytes()
                .len(),
            32
        );
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 3);
        assert_eq!(stats.inserts, 3);
        assert_eq!(stats.live_entries, 3);
        Ok(())
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
                    cursor_exhausted: false,
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
    fn backfill_batch_does_not_publish_until_cursor_exhausted() -> Result<()> {
        let temp = tempdir()?;
        let mut manifest = SemanticManifest::default();
        let indexer = SemanticIndexer::new("hash", None)?;
        let db_fingerprint = "db-fp-backfill-cursor-not-exhausted";

        let first = vec![EmbeddingInput::new(30, "first cursor batch")];
        let first_outcome = indexer.run_backfill_batch(
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
                cursor_exhausted: false,
            },
        )?;
        anyhow::ensure!(!first_outcome.published, "first batch must not publish");
        anyhow::ensure!(
            first_outcome.checkpoint_saved,
            "first batch should save a checkpoint"
        );

        let second = vec![EmbeddingInput::new(31, "second cursor batch")];
        let second_outcome = indexer.run_backfill_batch(
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
                cursor_exhausted: false,
            },
        )?;

        anyhow::ensure!(
            !second_outcome.published,
            "count-based completion would publish here; cursor state must win"
        );
        anyhow::ensure!(
            second_outcome.checkpoint_saved,
            "second batch should save a checkpoint"
        );
        anyhow::ensure!(
            !vector_index_path(temp.path(), indexer.embedder_id()).exists(),
            "non-exhausted cursor must not publish the final vector index"
        );
        let Some(checkpoint) = manifest.checkpoint.as_ref() else {
            bail!("checkpoint should remain after a non-exhausted cursor");
        };
        anyhow::ensure!(
            checkpoint.conversations_processed == 2,
            "wanted 2 processed conversations, got {}",
            checkpoint.conversations_processed
        );
        anyhow::ensure!(
            checkpoint.total_conversations == 2,
            "checkpoint should preserve the real DB total, got {}",
            checkpoint.total_conversations
        );
        anyhow::ensure!(
            !checkpoint.cursor_exhausted,
            "saved checkpoint should preserve the non-exhausted cursor state"
        );
        anyhow::ensure!(
            !checkpoint.is_complete(),
            "non-exhausted cursor checkpoint must not be complete"
        );
        anyhow::ensure!(
            checkpoint.progress_pct() == 99,
            "non-exhausted cursor checkpoint should report 99% progress, got {}",
            checkpoint.progress_pct()
        );
        anyhow::ensure!(
            second_outcome.conversations_processed == 2,
            "wanted outcome to report 2 processed conversations, got {}",
            second_outcome.conversations_processed
        );
        anyhow::ensure!(
            second_outcome.total_conversations == 2,
            "wanted outcome to preserve total 2, got {}",
            second_outcome.total_conversations
        );
        let progress_pct = second_outcome.progress_pct();
        anyhow::ensure!(
            (progress_pct - 99.0).abs() < f64::EPSILON,
            "non-published outcome should cap progress below 100%, got {}",
            progress_pct
        );
        Ok(())
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
                    cursor_exhausted: false,
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
                    cursor_exhausted: true,
                },
            )
            .unwrap();

        assert!(second_outcome.published);
        assert!(!second_outcome.checkpoint_saved);
        assert!((second_outcome.progress_pct() - 100.0).abs() < f64::EPSILON);
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
    fn backfill_publish_compacts_resumed_wal_before_rename() {
        let temp = tempdir().unwrap();
        let mut manifest = SemanticManifest::default();
        let indexer = SemanticIndexer::new("hash", None).unwrap();
        let db_fingerprint = "db-fp-backfill-small-resume";
        let first: Vec<EmbeddingInput> = (0..20)
            .map(|idx| EmbeddingInput::new(100 + idx, format!("first batch message {idx}")))
            .collect();

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
                    cursor_exhausted: false,
                },
            )
            .unwrap();
        assert!(first_outcome.checkpoint_saved);

        let second = vec![EmbeddingInput::new(200, "small final resume batch")];
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
                    cursor_exhausted: true,
                },
            )
            .unwrap();

        assert!(second_outcome.published);
        let final_path = vector_index_path(temp.path(), indexer.embedder_id());
        let mut final_wal_path = final_path.as_os_str().to_os_string();
        final_wal_path.push(".wal");
        assert!(!PathBuf::from(final_wal_path).exists());

        let published_index = FsVectorIndex::open(&final_path).unwrap();
        assert_eq!(published_index.record_count(), 21);
        let artifact = manifest.fast_tier.as_ref().expect("published fast tier");
        assert_eq!(artifact.doc_count, 21);
        assert_eq!(artifact.conversation_count, 2);
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
    fn canonical_embedding_batch_uses_conversation_packet_semantic_projection() -> Result<()> {
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
            &test_conversation_with_messages(
                "packet-projection",
                vec![
                    Message {
                        id: None,
                        idx: 0,
                        role: MessageRole::User,
                        author: None,
                        created_at: Some(1_700_000_000_500),
                        content: "user semantic text".to_string(),
                        extra_json: json!({}),
                        snippets: Vec::new(),
                    },
                    Message {
                        id: None,
                        idx: 1,
                        role: MessageRole::Tool,
                        author: None,
                        created_at: Some(1_700_000_000_600),
                        content: "tool semantic text".to_string(),
                        extra_json: json!({}),
                        snippets: Vec::new(),
                    },
                    Message {
                        id: None,
                        idx: 2,
                        role: MessageRole::System,
                        author: None,
                        created_at: Some(1_700_000_000_700),
                        content: String::new(),
                        extra_json: json!({}),
                        snippets: Vec::new(),
                    },
                ],
            ),
        )?;

        let batch = fetch_canonical_embedding_batch(&storage, 0, 1)?;

        assert_eq!(batch.conversations_in_batch, 1);
        assert_eq!(batch.inputs.len(), 2);
        assert_eq!(batch.inputs[0].content, "user semantic text");
        assert_eq!(batch.inputs[1].content, "tool semantic text");
        assert_eq!(batch.inputs[0].role, role_code_from_str("user").unwrap());
        assert_eq!(batch.inputs[1].role, role_code_from_str("tool").unwrap());
        let normalized_source_id =
            normalized_index_source_id(Some("remote-laptop"), None, Some("builder-host"));
        let expected_hash = crc32fast::hash(normalized_source_id.as_bytes());
        assert_eq!(batch.inputs[0].source_id, expected_hash);
        assert_eq!(batch.inputs[1].source_id, expected_hash);
        Ok(())
    }

    #[test]
    fn canonical_embedding_batch_pushes_last_message_id_filter_into_selection() -> Result<()> {
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
            &test_conversation("before-watermark", "old semantic message"),
        )?;
        let watermark: i64 = storage.raw().query_row_map(
            "SELECT MAX(id) FROM messages",
            &[] as &[ParamValue],
            |row| row.get_typed(0),
        )?;
        storage.insert_conversation_tree(
            agent_id,
            None,
            &test_conversation("after-watermark", "new semantic message"),
        )?;

        let batch = fetch_canonical_embedding_batch_inner(&storage, 0, 8, Some(watermark))?;

        assert_eq!(
            batch.conversations_in_batch, 1,
            "last_message_id must be pushed into candidate selection so old conversations are not counted in the batch"
        );
        assert_eq!(batch.total_conversations, 2);
        anyhow::ensure!(
            batch.cursor_exhausted,
            "message-cursor selection should exhaust the remaining cursor"
        );
        assert_eq!(batch.inputs.len(), 1);
        assert_eq!(batch.inputs[0].content, "new semantic message");
        assert!(
            i64::try_from(batch.inputs[0].message_id).unwrap_or(i64::MAX) > watermark,
            "selected semantic input must be strictly newer than the checkpoint message id"
        );
        Ok(())
    }

    #[test]
    fn semantic_conversation_total_uses_the_production_schema_aggregate() -> Result<()> {
        let temp = tempdir()?;
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
            &test_conversation("counted-first", "first counted semantic message"),
        )?;
        storage.insert_conversation_tree(
            agent_id,
            None,
            &test_conversation("counted-second", "second counted semantic message"),
        )?;
        storage.insert_conversation_tree(
            agent_id,
            None,
            &test_conversation_with_messages("empty-not-counted", Vec::new()),
        )?;

        // Legacy imports may have messages without either tail cache. Preserve
        // exactness by forcing one populated conversation through the residual
        // indexed point-probe path.
        let first_conversation_id: i64 = storage.raw().query_row_map(
            "SELECT MIN(id) FROM conversations",
            &[] as &[ParamValue],
            |row| row.get_typed(0),
        )?;
        let params = [ParamValue::from(first_conversation_id)];
        storage.raw().execute_compat(
            "UPDATE conversations SET last_message_idx = NULL WHERE id = ?1",
            &params,
        )?;
        storage.raw().execute_compat(
            "UPDATE conversation_tail_state
             SET last_message_idx = NULL
             WHERE conversation_id = ?1",
            &params,
        )?;

        let production_columns: Vec<String> = storage.raw().query_map_collect(
            "PRAGMA table_info(conversations)",
            &[] as &[ParamValue],
            |row| row.get_typed(1),
        )?;
        assert!(
            !production_columns
                .iter()
                .any(|name| name == "message_count"),
            "the regression fixture must use the production schema without conversations.message_count"
        );
        assert_eq!(total_semantic_conversations(&storage)?, 2);
        Ok(())
    }

    #[test]
    fn semantic_total_cache_is_scoped_to_the_active_build_identity() {
        let mut manifest = SemanticManifest {
            checkpoint: Some(BuildCheckpoint {
                tier: TierKind::Fast,
                embedder_id: "fnv1a-384".to_string(),
                last_offset: 91,
                docs_embedded: 1_247,
                conversations_processed: 91,
                total_conversations: 15_246,
                db_fingerprint: "content-v1:432504:1:432504".to_string(),
                schema_version: SEMANTIC_SCHEMA_VERSION,
                chunking_version: CHUNKING_STRATEGY_VERSION,
                saved_at_ms: 1,
                last_message_id: Some(1_250),
                cursor_exhausted: false,
            }),
            ..SemanticManifest::default()
        };

        assert_eq!(
            cached_semantic_total_conversations(
                &manifest,
                TierKind::Fast,
                "fnv1a-384",
                "content-v1:432504:1:432504"
            ),
            Some((15_246, "checkpoint"))
        );
        assert_eq!(
            cached_semantic_total_conversations(
                &manifest,
                TierKind::Quality,
                "minilm-384",
                "content-v1:432504:1:432504"
            ),
            None,
            "a different tier/embedder must not inherit an unrelated checkpoint total"
        );
        assert_eq!(
            cached_semantic_total_conversations(
                &manifest,
                TierKind::Fast,
                "fnv1a-384",
                "content-v1:432505:1:432505"
            ),
            None,
            "a changed DB fingerprint must force a fresh aggregate"
        );
        manifest
            .checkpoint
            .as_mut()
            .expect("checkpoint")
            .schema_version = SEMANTIC_SCHEMA_VERSION.saturating_add(1);
        assert_eq!(
            cached_semantic_total_conversations(
                &manifest,
                TierKind::Fast,
                "fnv1a-384",
                "content-v1:432504:1:432504"
            ),
            None,
            "an incompatible semantic schema must invalidate the cached total"
        );
    }

    #[test]
    fn resumed_semantic_candidate_selection_is_cursor_bounded() -> Result<()> {
        let temp = tempdir()?;
        let db_path = temp.path().join("agent_search.db");
        let storage = FrankenStorage::open(&db_path)?;
        let agent_id = storage.ensure_agent(&Agent {
            id: None,
            slug: "codex".to_string(),
            name: "Codex".to_string(),
            version: None,
            kind: AgentKind::Cli,
        })?;
        for index in 0..192 {
            storage.insert_conversation_tree(
                agent_id,
                None,
                &test_conversation(
                    &format!("resume-before-{index}"),
                    &format!("old semantic message {index}"),
                ),
            )?;
        }
        let checkpoint_conversation_id: i64 = storage.raw().query_row_map(
            "SELECT MAX(id) FROM conversations",
            &[] as &[ParamValue],
            |row| row.get_typed(0),
        )?;
        let checkpoint_message_id: i64 = storage.raw().query_row_map(
            "SELECT MAX(id) FROM messages",
            &[] as &[ParamValue],
            |row| row.get_typed(0),
        )?;
        for index in 0..128 {
            storage.insert_conversation_tree(
                agent_id,
                None,
                &test_conversation(
                    &format!("resume-after-{index}"),
                    &format!("new semantic message {index}"),
                ),
            )?;
        }

        let total = total_semantic_conversations(&storage)?;
        assert_eq!(total, 320);
        let started = Instant::now();
        let batch = fetch_canonical_embedding_batch_inner_with_caps_and_total(
            &storage,
            checkpoint_conversation_id,
            64,
            Some(checkpoint_message_id),
            SemanticCheckpointCaps::unlimited(),
            total,
            None,
        )?;

        assert_eq!(batch.conversations_in_batch, 64);
        assert_eq!(batch.inputs.len(), 64);
        assert_eq!(batch.total_conversations, 320);
        assert!(!batch.cursor_exhausted);
        assert!(
            batch.inputs.iter().all(|input| {
                i64::try_from(input.message_id).unwrap_or(i64::MAX) > checkpoint_message_id
            }),
            "every selected message must advance past the durable message cursor"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "a 64-conversation resume must not replay the whole canonical corpus"
        );
        Ok(())
    }

    #[test]
    fn resumed_semantic_candidate_selection_streams_across_sparse_parent_gap() -> Result<()> {
        const CHECKPOINT_CONVERSATION_ID: i64 = 1;
        const CHECKPOINT_MESSAGE_ID: i64 = 6_669;
        const FIRST_ELIGIBLE_CONVERSATION_ID: i64 = 6_670;
        const SECOND_ELIGIBLE_CONVERSATION_ID: i64 = 6_671;
        const EARLY_MESSAGE_LATE_CONVERSATION_ID: i64 = 6_672;

        fn linux_rss_bytes() -> Option<u64> {
            let status = std::fs::read_to_string("/proc/self/status").ok()?;
            let rss_kib = status.lines().find_map(|line| {
                line.strip_prefix("VmRSS:")?
                    .split_whitespace()
                    .next()?
                    .parse::<u64>()
                    .ok()
            })?;
            rss_kib.checked_mul(1_024)
        }

        let temp = tempdir()?;
        let db_path = temp.path().join("agent_search.db");
        let bootstrap_storage = FrankenStorage::open(&db_path)?;
        let agent_id = bootstrap_storage.ensure_agent(&Agent {
            id: None,
            slug: "codex".to_string(),
            name: "Codex".to_string(),
            version: None,
            kind: AgentKind::Cli,
        })?;
        // Reproduce #348's production shape without spending the test budget
        // on fixture construction through thousands of individual commits.
        // Conversations 2..=6,669 are newer parents whose messages are all at
        // or below the durable message cursor: 6,668 consecutive misses before
        // the first eligible conversation in conversation-id order.
        // Seed through FrankenSQLite itself (the project's sole SQLite engine).
        // Multi-row statements are chunked so fixture construction stays
        // parser-bounded while one outer transaction avoids thousands of
        // fsyncs. Every interpolated value below is an integer controlled by
        // this test; no external string enters the SQL.
        use std::fmt::Write as _;
        const SEED_ROWS_PER_STATEMENT: i64 = 256;
        let mut seed_sql = String::from("BEGIN IMMEDIATE;");
        let mut chunk_start = CHECKPOINT_CONVERSATION_ID;
        while chunk_start <= EARLY_MESSAGE_LATE_CONVERSATION_ID {
            let chunk_end =
                (chunk_start + SEED_ROWS_PER_STATEMENT - 1).min(EARLY_MESSAGE_LATE_CONVERSATION_ID);
            seed_sql.push_str(
                "INSERT INTO conversations(
                     id, agent_id, source_id, external_id, source_path
                 ) VALUES",
            );
            for conversation_id in chunk_start..=chunk_end {
                if conversation_id > chunk_start {
                    seed_sql.push(',');
                }
                write!(
                    seed_sql,
                    "({conversation_id},{agent_id},'local','cass-348-sparse-{conversation_id}',\
                     '/tmp/cass-348/{conversation_id}.jsonl')"
                )
                .expect("writing fixture SQL to String cannot fail");
            }
            seed_sql.push(';');

            seed_sql.push_str(
                "INSERT INTO conversation_tail_state(conversation_id, last_message_idx) VALUES",
            );
            for conversation_id in chunk_start..=chunk_end {
                if conversation_id > chunk_start {
                    seed_sql.push(',');
                }
                write!(seed_sql, "({conversation_id},0)")
                    .expect("writing fixture SQL to String cannot fail");
            }
            seed_sql.push(';');
            chunk_start = chunk_end + 1;
        }

        let mut message_chunk_start = 2_i64;
        while message_chunk_start < FIRST_ELIGIBLE_CONVERSATION_ID {
            let message_chunk_end = (message_chunk_start + SEED_ROWS_PER_STATEMENT - 1)
                .min(FIRST_ELIGIBLE_CONVERSATION_ID - 1);
            seed_sql.push_str(
                "INSERT INTO messages(
                     id, conversation_id, idx, role, content
                 ) VALUES",
            );
            for conversation_id in message_chunk_start..=message_chunk_end {
                if conversation_id > message_chunk_start {
                    seed_sql.push(',');
                }
                let message_id = conversation_id - 1;
                write!(
                    seed_sql,
                    "({message_id},{conversation_id},0,'user','sparse semantic resume row')"
                )
                .expect("writing fixture SQL to String cannot fail");
            }
            seed_sql.push(';');
            message_chunk_start = message_chunk_end + 1;
        }

        // Global message order intentionally disagrees with conversation
        // order. A correct bounded selector must evict 6,672 and return
        // 6,670/6,671, otherwise advancing the conversation checkpoint would
        // permanently skip an older eligible conversation.
        write!(
            seed_sql,
            "INSERT INTO messages(id, conversation_id, idx, role, content) VALUES
             ({CHECKPOINT_MESSAGE_ID},{CHECKPOINT_CONVERSATION_ID},0,'user','checkpoint row'),
             ({},{EARLY_MESSAGE_LATE_CONVERSATION_ID},0,'user','eligible row'),
             ({},{FIRST_ELIGIBLE_CONVERSATION_ID},0,'user','eligible row'),
             ({},{SECOND_ELIGIBLE_CONVERSATION_ID},0,'user','eligible row');COMMIT;",
            CHECKPOINT_MESSAGE_ID + 1,
            CHECKPOINT_MESSAGE_ID + 2,
            CHECKPOINT_MESSAGE_ID + 3,
        )
        .expect("writing fixture SQL to String cannot fail");
        bootstrap_storage.raw().execute_batch(&seed_sql)?;
        drop(bootstrap_storage);

        let storage = FrankenStorage::open(&db_path)?;
        let rss_before = linux_rss_bytes();
        let started = Instant::now();
        let (selected, eligible_message_rows_scanned) =
            fetch_bounded_semantic_candidate_conversation_ids(
                &storage,
                CHECKPOINT_CONVERSATION_ID,
                Some(CHECKPOINT_MESSAGE_ID),
                2,
            )?;
        let elapsed = started.elapsed();
        let rss_after = linux_rss_bytes();

        assert_eq!(
            selected,
            [
                FIRST_ELIGIBLE_CONVERSATION_ID,
                SECOND_ELIGIBLE_CONVERSATION_ID
            ]
        );
        assert_eq!(
            eligible_message_rows_scanned, 3,
            "the selector should stream only eligible post-cursor messages, not execute 6,668 parent probes"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "sparse-gap selection took {elapsed:?}"
        );
        if let (Some(before), Some(after)) = (rss_before, rss_after) {
            assert!(
                after.saturating_sub(before) < 256 * 1024 * 1024,
                "sparse-gap selection grew RSS by {} bytes",
                after.saturating_sub(before)
            );
        }
        Ok(())
    }

    #[test]
    #[ignore = "large 15,246-conversation / 432,504-message acceptance proof for cass#343"]
    fn resumed_semantic_selection_matches_large_production_shape() -> Result<()> {
        const CONVERSATION_COUNT: i64 = 15_246;
        const MESSAGE_COUNT: i64 = 432_504;
        const CONVERSATIONS_WITH_EXTRA_MESSAGE: i64 = MESSAGE_COUNT - (CONVERSATION_COUNT * 28);

        fn linux_rss_bytes() -> Option<u64> {
            let status = std::fs::read_to_string("/proc/self/status").ok()?;
            let rss_kib = status.lines().find_map(|line| {
                line.strip_prefix("VmRSS:")?
                    .split_whitespace()
                    .next()?
                    .parse::<u64>()
                    .ok()
            })?;
            rss_kib.checked_mul(1_024)
        }

        let temp = tempdir()?;
        let db_path = temp.path().join("agent_search.db");
        let bootstrap_storage = FrankenStorage::open(&db_path)?;
        let agent_id = bootstrap_storage.ensure_agent(&Agent {
            id: None,
            slug: "codex".to_string(),
            name: "Codex".to_string(),
            version: None,
            kind: AgentKind::Cli,
        })?;
        drop(bootstrap_storage);

        // Fixture construction is deliberately outside the measured region.
        // Use the repo's existing C-SQLite interop dependency and prepared
        // statements so creating 432k rows does not dominate this ignored
        // acceptance test. The actual count and selection proof below reopens
        // the database through FrankenStorage/FrankenSQLite.
        let mut sqlite = rusqlite::Connection::open(&db_path)?;
        let tx = sqlite.transaction()?;
        let mut message_id = 1_i64;
        {
            let mut insert_conversation = tx.prepare(
                "INSERT INTO conversations(
                     id, agent_id, source_id, external_id, source_path
                 ) VALUES(?1, ?2, 'local', ?3, ?4)",
            )?;
            let mut insert_message = tx.prepare(
                "INSERT INTO messages(
                     id, conversation_id, idx, role, content
                 ) VALUES(?1, ?2, ?3, 'user', 'semantic acceptance row')",
            )?;
            let mut insert_tail = tx.prepare(
                "INSERT INTO conversation_tail_state(conversation_id, last_message_idx)
                 VALUES(?1, ?2)",
            )?;
            for conversation_id in 1..=CONVERSATION_COUNT {
                let external_id = format!("cass-343-large-{conversation_id}");
                let source_path = format!("/tmp/cass-343/{conversation_id}.jsonl");
                insert_conversation.execute(rusqlite::params![
                    conversation_id,
                    agent_id,
                    external_id,
                    source_path
                ])?;
                let messages_in_conversation =
                    28 + i64::from(conversation_id <= CONVERSATIONS_WITH_EXTRA_MESSAGE);
                for message_index in 0..messages_in_conversation {
                    insert_message.execute(rusqlite::params![
                        message_id,
                        conversation_id,
                        message_index
                    ])?;
                    message_id = message_id.saturating_add(1);
                }
                insert_tail.execute(rusqlite::params![
                    conversation_id,
                    messages_in_conversation - 1
                ])?;
            }
        }
        tx.commit()?;
        drop(sqlite);
        assert_eq!(message_id - 1, MESSAGE_COUNT);

        let storage = FrankenStorage::open(&db_path)?;
        let count_started = Instant::now();
        let total = total_semantic_conversations(&storage)?;
        let count_elapsed = count_started.elapsed();
        assert_eq!(total, u64::try_from(CONVERSATION_COUNT)?);

        let rss_before = linux_rss_bytes();
        let selection_started = Instant::now();
        let batch = fetch_canonical_embedding_batch_inner_with_caps_and_total(
            &storage,
            91,
            64,
            Some(1_250),
            SemanticCheckpointCaps::unlimited(),
            total,
            None,
        )?;
        let selection_elapsed = selection_started.elapsed();
        let rss_after = linux_rss_bytes();

        eprintln!(
            "CASS_343_LARGE_SELECTION conversations={CONVERSATION_COUNT} messages={MESSAGE_COUNT} count_ms={} selection_ms={} rss_before={rss_before:?} rss_after={rss_after:?}",
            count_elapsed.as_millis(),
            selection_elapsed.as_millis()
        );
        assert_eq!(batch.conversations_in_batch, 64);
        assert_eq!(batch.inputs.len(), 64 * 29);
        assert_eq!(batch.total_conversations, 15_246);
        assert!(!batch.cursor_exhausted);
        assert!(
            batch.inputs.iter().all(|input| input.message_id > 1_250),
            "the durable nonzero message cursor must be applied before materialization"
        );
        assert!(
            count_elapsed < std::time::Duration::from_secs(30),
            "production-schema aggregate must not clone/execute and materialize per outer row"
        );
        assert!(
            selection_elapsed < std::time::Duration::from_secs(30),
            "64-conversation candidate selection must stay bounded on the 432k-message shape"
        );
        if let (Some(before), Some(after)) = (rss_before, rss_after) {
            assert!(
                after.saturating_sub(before) < 256 * 1024 * 1024,
                "bounded resume selection grew RSS by {} bytes",
                after.saturating_sub(before)
            );
        }
        Ok(())
    }

    #[test]
    fn canonical_embedding_batch_reports_unexhausted_cursor_at_sql_limit() -> Result<()> {
        let temp = tempdir()?;
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
            &test_conversation("limit-first", "first limit semantic message"),
        )?;
        storage.insert_conversation_tree(
            agent_id,
            None,
            &test_conversation("limit-second", "second limit semantic message"),
        )?;

        let first = fetch_canonical_embedding_batch_inner(&storage, 0, 1, None)?;
        anyhow::ensure!(
            first.conversations_in_batch == 1,
            "wanted first batch to select 1 conversation, got {}",
            first.conversations_in_batch
        );
        anyhow::ensure!(
            !first.cursor_exhausted,
            "over-fetching one candidate should prove another conversation remains"
        );

        let second =
            fetch_canonical_embedding_batch_inner(&storage, first.last_conversation_id, 1, None)?;
        anyhow::ensure!(
            second.conversations_in_batch == 1,
            "wanted second batch to select 1 conversation, got {}",
            second.conversations_in_batch
        );
        anyhow::ensure!(
            second.cursor_exhausted,
            "the final page should report cursor exhaustion even when it exactly fills the requested limit"
        );
        Ok(())
    }

    #[test]
    fn canonical_embedding_visitor_replays_every_bounded_page() -> Result<()> {
        let temp = tempdir()?;
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
            &test_conversation("visitor-first", "first visitor semantic message"),
        )?;
        storage.insert_conversation_tree(
            agent_id,
            None,
            &test_conversation("visitor-second", "second visitor semantic message"),
        )?;

        let mut contents = Vec::new();
        visit_packet_embedding_inputs_from_storage_with_limits(
            &storage,
            1,
            SemanticCheckpointCaps {
                max_messages: 1,
                max_bytes: 1,
            },
            |input| {
                contents.push(input.content);
                Ok(())
            },
        )?;

        assert_eq!(
            contents,
            [
                "first visitor semantic message",
                "second visitor semantic message"
            ]
        );
        Ok(())
    }

    #[test]
    fn checkpoint_caps_stop_at_whole_conversation_boundary() -> Result<()> {
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
        for external_id in ["cap-first", "cap-second", "cap-third"] {
            storage.insert_conversation_tree(
                agent_id,
                None,
                &test_conversation_with_messages(
                    external_id,
                    vec![
                        Message {
                            id: None,
                            idx: 0,
                            role: MessageRole::User,
                            author: None,
                            created_at: Some(1_700_000_000_500),
                            content: format!("{external_id} user semantic text"),
                            extra_json: json!({}),
                            snippets: Vec::new(),
                        },
                        Message {
                            id: None,
                            idx: 1,
                            role: MessageRole::Agent,
                            author: None,
                            created_at: Some(1_700_000_000_600),
                            content: format!("{external_id} assistant semantic text"),
                            extra_json: json!({}),
                            snippets: Vec::new(),
                        },
                    ],
                ),
            )?;
        }

        let first_conversation_id: i64 = storage.raw().query_row_map(
            "SELECT MIN(id) FROM conversations",
            &[] as &[ParamValue],
            |row| row.get_typed(0),
        )?;
        let batch = fetch_canonical_embedding_batch_inner_with_caps(
            &storage,
            0,
            8,
            None,
            SemanticCheckpointCaps {
                max_messages: 3,
                max_bytes: 0,
            },
        )?;

        assert_eq!(batch.conversations_in_batch, 1);
        anyhow::ensure!(
            !batch.cursor_exhausted,
            "checkpoint caps that stop before the next whole conversation must not publish"
        );
        assert_eq!(batch.last_conversation_id, first_conversation_id);
        assert_eq!(batch.inputs.len(), 2);
        assert!(
            batch
                .inputs
                .iter()
                .all(|input| input.content.contains("cap-first"))
        );
        assert_eq!(batch.total_conversations, 3);
        Ok(())
    }

    #[test]
    fn packet_embedding_inputs_from_storage_since_only_emits_new_canonical_messages() -> Result<()>
    {
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
            &test_conversation_with_messages(
                "packet-delta",
                vec![
                    Message {
                        id: None,
                        idx: 0,
                        role: MessageRole::User,
                        author: None,
                        created_at: Some(1_700_000_000_500),
                        content: "existing semantic text".to_string(),
                        extra_json: json!({}),
                        snippets: Vec::new(),
                    },
                    Message {
                        id: None,
                        idx: 1,
                        role: MessageRole::Agent,
                        author: None,
                        created_at: Some(1_700_000_000_600),
                        content: "existing assistant text".to_string(),
                        extra_json: json!({}),
                        snippets: Vec::new(),
                    },
                ],
            ),
        )?;
        let watermark: i64 = storage.raw().query_row_map(
            "SELECT MAX(id) FROM messages",
            &[] as &[ParamValue],
            |row| row.get_typed(0),
        )?;

        storage.insert_conversation_tree(
            agent_id,
            None,
            &test_conversation_with_messages(
                "packet-delta",
                vec![
                    Message {
                        id: None,
                        idx: 0,
                        role: MessageRole::User,
                        author: None,
                        created_at: Some(1_700_000_000_500),
                        content: "existing semantic text".to_string(),
                        extra_json: json!({}),
                        snippets: Vec::new(),
                    },
                    Message {
                        id: None,
                        idx: 1,
                        role: MessageRole::Agent,
                        author: None,
                        created_at: Some(1_700_000_000_600),
                        content: "existing assistant text".to_string(),
                        extra_json: json!({}),
                        snippets: Vec::new(),
                    },
                    Message {
                        id: None,
                        idx: 2,
                        role: MessageRole::Agent,
                        author: None,
                        created_at: Some(1_700_000_000_700),
                        content: "new packet semantic text".to_string(),
                        extra_json: json!({}),
                        snippets: Vec::new(),
                    },
                    Message {
                        id: None,
                        idx: 3,
                        role: MessageRole::System,
                        author: None,
                        created_at: Some(1_700_000_000_800),
                        content: String::new(),
                        extra_json: json!({}),
                        snippets: Vec::new(),
                    },
                ],
            ),
        )?;

        let batch = packet_embedding_inputs_from_storage_since(&storage, watermark)?;

        assert_eq!(batch.conversations_in_batch, 1);
        assert_eq!(batch.inputs.len(), 1);
        assert_eq!(batch.inputs[0].content, "new packet semantic text");
        assert_eq!(
            batch.inputs[0].role,
            role_code_from_str("assistant").unwrap()
        );
        let normalized_source_id =
            normalized_index_source_id(Some("remote-laptop"), None, Some("builder-host"));
        assert_eq!(
            batch.inputs[0].source_id,
            crc32fast::hash(normalized_source_id.as_bytes())
        );
        let expected_raw_max_id: i64 = storage.raw().query_row_map(
            "SELECT MAX(id) FROM messages",
            &[] as &[ParamValue],
            |row| row.get_typed(0),
        )?;
        assert_eq!(batch.raw_max_message_id, Some(expected_raw_max_id));
        Ok(())
    }

    #[test]
    fn packet_catch_up_emits_expected_semantic_docs_after_watermark() -> Result<()> {
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
        let workspace_id = storage.ensure_workspace(Path::new("/tmp/workspace"), None)?;

        storage.insert_conversation_tree(
            agent_id,
            Some(workspace_id),
            &test_conversation_with_messages(
                "legacy-packet-semantics",
                vec![
                    Message {
                        id: None,
                        idx: 0,
                        role: MessageRole::User,
                        author: None,
                        created_at: Some(1_700_000_000_500),
                        content: "before watermark".to_string(),
                        extra_json: json!({}),
                        snippets: Vec::new(),
                    },
                    Message {
                        id: None,
                        idx: 1,
                        role: MessageRole::Agent,
                        author: None,
                        created_at: Some(1_700_000_000_600),
                        content: "before watermark assistant".to_string(),
                        extra_json: json!({}),
                        snippets: Vec::new(),
                    },
                ],
            ),
        )?;

        let watermark: i64 = storage.raw().query_row_map(
            "SELECT MAX(id) FROM messages",
            &[] as &[ParamValue],
            |row| row.get_typed(0),
        )?;

        storage.insert_conversation_tree(
            agent_id,
            Some(workspace_id),
            &test_conversation_with_messages(
                "legacy-packet-semantics",
                vec![
                    Message {
                        id: None,
                        idx: 0,
                        role: MessageRole::User,
                        author: None,
                        created_at: Some(1_700_000_000_500),
                        content: "before watermark".to_string(),
                        extra_json: json!({}),
                        snippets: Vec::new(),
                    },
                    Message {
                        id: None,
                        idx: 1,
                        role: MessageRole::Agent,
                        author: None,
                        created_at: Some(1_700_000_000_600),
                        content: "before watermark assistant".to_string(),
                        extra_json: json!({}),
                        snippets: Vec::new(),
                    },
                    Message {
                        id: None,
                        idx: 2,
                        role: MessageRole::Agent,
                        author: None,
                        created_at: Some(1_700_000_000_700),
                        content: "after watermark assistant".to_string(),
                        extra_json: json!({}),
                        snippets: Vec::new(),
                    },
                    Message {
                        id: None,
                        idx: 3,
                        role: MessageRole::System,
                        author: None,
                        created_at: Some(1_700_000_000_800),
                        content: String::new(),
                        extra_json: json!({}),
                        snippets: Vec::new(),
                    },
                ],
            ),
        )?;
        storage.insert_conversation_tree(
            agent_id,
            Some(workspace_id),
            &test_conversation_with_messages(
                "legacy-packet-semantics-second-conv",
                vec![
                    Message {
                        id: None,
                        idx: 0,
                        role: MessageRole::Tool,
                        author: None,
                        created_at: Some(1_700_000_000_900),
                        content: "after watermark tool".to_string(),
                        extra_json: json!({}),
                        snippets: Vec::new(),
                    },
                    Message {
                        id: None,
                        idx: 1,
                        role: MessageRole::System,
                        author: None,
                        created_at: Some(1_700_000_001_000),
                        content: String::new(),
                        extra_json: json!({}),
                        snippets: Vec::new(),
                    },
                ],
            ),
        )?;

        let packet_batch = packet_embedding_inputs_from_storage_since(&storage, watermark)?;
        let normalized_source_id =
            normalized_index_source_id(Some("remote-laptop"), None, Some("builder-host"));
        let source_id_hash = crc32fast::hash(normalized_source_id.as_bytes());
        let expected = vec![
            ComparableSemanticInput {
                message_id: u64::try_from(watermark + 1).unwrap(),
                created_at_ms: 1_700_000_000_700,
                agent_id: u32::try_from(agent_id).unwrap(),
                workspace_id: u32::try_from(workspace_id).unwrap(),
                source_id: source_id_hash,
                role: role_code_from_str("assistant").unwrap(),
                content: "after watermark assistant".to_string(),
            },
            ComparableSemanticInput {
                message_id: u64::try_from(watermark + 3).unwrap(),
                created_at_ms: 1_700_000_000_900,
                agent_id: u32::try_from(agent_id).unwrap(),
                workspace_id: u32::try_from(workspace_id).unwrap(),
                source_id: source_id_hash,
                role: role_code_from_str("tool").unwrap(),
                content: "after watermark tool".to_string(),
            },
        ];

        assert_eq!(comparable_semantic_inputs(packet_batch.inputs), expected);
        assert_eq!(packet_batch.conversations_in_batch, 2);
        assert_eq!(packet_batch.raw_max_message_id, Some(watermark + 4));
        Ok(())
    }

    #[test]
    fn packet_embedding_inputs_for_message_ids_matches_since_selection() -> Result<()> {
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
        let workspace_id = storage.ensure_workspace(Path::new("/tmp/workspace"), None)?;

        storage.insert_conversation_tree(
            agent_id,
            Some(workspace_id),
            &test_conversation_with_messages(
                "selected-vs-since",
                vec![
                    Message {
                        id: None,
                        idx: 0,
                        role: MessageRole::User,
                        author: None,
                        created_at: Some(1_700_000_100_100),
                        content: "before watermark".to_string(),
                        extra_json: json!({}),
                        snippets: Vec::new(),
                    },
                    Message {
                        id: None,
                        idx: 1,
                        role: MessageRole::Agent,
                        author: None,
                        created_at: Some(1_700_000_100_200),
                        content: "before watermark assistant".to_string(),
                        extra_json: json!({}),
                        snippets: Vec::new(),
                    },
                ],
            ),
        )?;

        let watermark: i64 = storage.raw().query_row_map(
            "SELECT MAX(id) FROM messages",
            &[] as &[ParamValue],
            |row| row.get_typed(0),
        )?;

        storage.insert_conversation_tree(
            agent_id,
            Some(workspace_id),
            &test_conversation_with_messages(
                "selected-vs-since",
                vec![
                    Message {
                        id: None,
                        idx: 0,
                        role: MessageRole::User,
                        author: None,
                        created_at: Some(1_700_000_100_100),
                        content: "before watermark".to_string(),
                        extra_json: json!({}),
                        snippets: Vec::new(),
                    },
                    Message {
                        id: None,
                        idx: 1,
                        role: MessageRole::Agent,
                        author: None,
                        created_at: Some(1_700_000_100_200),
                        content: "before watermark assistant".to_string(),
                        extra_json: json!({}),
                        snippets: Vec::new(),
                    },
                    Message {
                        id: None,
                        idx: 2,
                        role: MessageRole::Tool,
                        author: None,
                        created_at: Some(1_700_000_100_300),
                        content: "after watermark tool".to_string(),
                        extra_json: json!({}),
                        snippets: Vec::new(),
                    },
                    Message {
                        id: None,
                        idx: 3,
                        role: MessageRole::System,
                        author: None,
                        created_at: Some(1_700_000_100_400),
                        content: String::new(),
                        extra_json: json!({}),
                        snippets: Vec::new(),
                    },
                ],
            ),
        )?;
        storage.insert_conversation_tree(
            agent_id,
            Some(workspace_id),
            &test_conversation_with_messages(
                "selected-vs-since-second",
                vec![Message {
                    id: None,
                    idx: 0,
                    role: MessageRole::Agent,
                    author: None,
                    created_at: Some(1_700_000_100_500),
                    content: "after watermark assistant".to_string(),
                    extra_json: json!({}),
                    snippets: Vec::new(),
                }],
            ),
        )?;

        let since_batch = packet_embedding_inputs_from_storage_since(&storage, watermark)?;
        let conversation_ids: Vec<i64> = storage.raw().query_map_collect(
            "SELECT DISTINCT conversation_id
             FROM messages
             WHERE id > ?1
             ORDER BY conversation_id ASC",
            &[ParamValue::from(watermark)],
            |row| row.get_typed(0),
        )?;
        let selected_message_ids: HashSet<i64> = storage
            .raw()
            .query_map_collect(
                "SELECT id
                 FROM messages
                 WHERE id > ?1
                 ORDER BY id ASC",
                &[ParamValue::from(watermark)],
                |row| row.get_typed(0),
            )?
            .into_iter()
            .collect();
        let selected_inputs = packet_embedding_inputs_from_storage_for_message_ids(
            &storage,
            &conversation_ids,
            &selected_message_ids,
        )?;

        assert_eq!(
            comparable_semantic_inputs(selected_inputs),
            comparable_semantic_inputs(since_batch.inputs)
        );
        Ok(())
    }

    #[test]
    fn default_batch_size_uses_new_value() {
        // The test setup must not leak a caller-provided CASS_SEMANTIC_BATCH_SIZE
        // override, which would mask the constant bump we're asserting on.
        let _guard = EnvVarGuard::remove("CASS_SEMANTIC_BATCH_SIZE");
        let indexer = SemanticIndexer::new("hash", None).unwrap();
        assert_eq!(indexer.batch_size(), DEFAULT_SEMANTIC_BATCH_SIZE);
    }

    #[test]
    fn semantic_watchdog_and_checkpoint_caps_have_derived_defaults() {
        let _warn = EnvVarGuard::remove("CASS_SEMANTIC_EMBED_BATCH_WARN_AFTER_MS");
        let _fail = EnvVarGuard::remove("CASS_SEMANTIC_EMBED_BATCH_FAIL_AFTER_MS");
        let _messages = EnvVarGuard::remove("CASS_SEMANTIC_MAX_MESSAGES_PER_CHECKPOINT");
        let _bytes = EnvVarGuard::remove("CASS_SEMANTIC_MAX_BYTES_PER_CHECKPOINT");

        assert_eq!(
            resolved_semantic_embed_batch_warn_after_ms(),
            DEFAULT_SEMANTIC_EMBED_BATCH_WARN_AFTER_MS
        );
        assert_eq!(
            resolved_semantic_embed_batch_fail_after_ms(),
            DEFAULT_SEMANTIC_EMBED_BATCH_FAIL_AFTER_MS
        );
        assert_eq!(
            SemanticCheckpointCaps::from_env(),
            SemanticCheckpointCaps {
                max_messages: DEFAULT_SEMANTIC_MAX_MESSAGES_PER_CHECKPOINT,
                max_bytes: DEFAULT_SEMANTIC_MAX_BYTES_PER_CHECKPOINT,
            }
        );
    }

    #[test]
    fn parallel_prep_enabled_reuses_truthy_env_parser() {
        for (value, expected) in [
            ("1", true),
            ("true", true),
            (" YeS ", true),
            ("on", true),
            ("0", false),
            ("false", false),
            ("off", false),
        ] {
            let _guard = EnvVarGuard::set("CASS_SEMANTIC_PREP_PARALLEL", value);
            assert_eq!(parallel_prep_enabled(), expected, "env value {value:?}");
        }

        let _guard = EnvVarGuard::remove("CASS_SEMANTIC_PREP_PARALLEL");
        assert!(!parallel_prep_enabled());
    }

    #[test]
    fn saturating_u64_from_usize_covers_bounds() {
        assert_eq!(saturating_u64_from_usize(0), 0);
        assert_eq!(saturating_u64_from_usize(42), 42);
        assert_eq!(
            saturating_u64_from_usize(usize::MAX),
            u64::try_from(usize::MAX).unwrap_or(u64::MAX)
        );
    }

    /// `coding_agent_session_search-ibuuh.32` (sink #3 equivalence gate):
    /// the packet-driven `semantic_inputs_from_packets` helper must
    /// produce the same `EmbeddingInput` list a fresh storage replay
    /// returns for the same canonical corpus. Once this passes, callers
    /// that already hold packets (rebuild pipeline, salvage replay,
    /// repair flows) can drive the semantic preparation consumer
    /// without a second canonical-row round-trip.
    #[test]
    fn issue_342_semantic_replay_reports_progress_and_matches_packets() -> Result<()> {
        let temp = tempdir().unwrap();
        let db_path = temp.path().join("agent_search.db");
        let storage = FrankenStorage::open(&db_path)?;

        let agent_id_codex = storage.ensure_agent(&Agent {
            id: None,
            slug: "codex".to_string(),
            name: "Codex".to_string(),
            version: None,
            kind: AgentKind::Cli,
        })?;
        let agent_id_claude = storage.ensure_agent(&Agent {
            id: None,
            slug: "claude_code".to_string(),
            name: "Claude Code".to_string(),
            version: None,
            kind: AgentKind::Cli,
        })?;
        let workspace_id =
            storage.ensure_workspace(Path::new("/tmp/semantic-equivalence-ws"), None)?;

        // Two conversations on different agents, mixed roles, including
        // an empty-content system message that the semantic projection
        // must filter (matches the legacy storage replay).
        storage.insert_conversation_tree(
            agent_id_codex,
            Some(workspace_id),
            &test_conversation_with_messages(
                "packet-equiv-1",
                vec![
                    Message {
                        id: None,
                        idx: 0,
                        role: MessageRole::User,
                        author: None,
                        created_at: Some(1_700_000_000_500),
                        content: "first user prompt".to_string(),
                        extra_json: json!({}),
                        snippets: Vec::new(),
                    },
                    Message {
                        id: None,
                        idx: 1,
                        role: MessageRole::Agent,
                        author: None,
                        created_at: Some(1_700_000_000_600),
                        content: "first assistant reply".to_string(),
                        extra_json: json!({}),
                        snippets: Vec::new(),
                    },
                    Message {
                        id: None,
                        idx: 2,
                        role: MessageRole::System,
                        author: None,
                        created_at: Some(1_700_000_000_700),
                        // Empty content is filtered by both paths.
                        content: String::new(),
                        extra_json: json!({}),
                        snippets: Vec::new(),
                    },
                ],
            ),
        )?;
        storage.insert_conversation_tree(
            agent_id_claude,
            Some(workspace_id),
            &test_conversation_with_messages(
                "packet-equiv-2",
                vec![
                    Message {
                        id: None,
                        idx: 0,
                        role: MessageRole::Tool,
                        author: Some("ripgrep".to_string()),
                        created_at: Some(1_700_000_001_500),
                        content: "tool output line".to_string(),
                        extra_json: json!({}),
                        snippets: Vec::new(),
                    },
                    Message {
                        id: None,
                        idx: 1,
                        role: MessageRole::Agent,
                        author: None,
                        created_at: Some(1_700_000_001_600),
                        content: "second assistant reply".to_string(),
                        extra_json: json!({}),
                        snippets: Vec::new(),
                    },
                ],
            ),
        )?;

        // Legacy path: the storage-driven replay that the rebuild
        // pipeline currently uses.
        let mut replay_progress = Vec::new();
        let storage_inputs =
            packet_embedding_inputs_from_storage_with_progress(&storage, |current, total| {
                replay_progress.push((current, total))
            })?;
        assert_eq!(replay_progress.last(), Some(&(2, 2)));

        // Packet-driven path: re-fetch the canonical envelopes (so we
        // get the storage-internal agent/workspace ids the rebuild path
        // would normally pair with packets), then convert those rows
        // into ConversationPackets via canonical replay and feed them
        // through `semantic_inputs_from_packets`.
        let conversation_ids: Vec<i64> = storage.raw().query_map_collect(
            "SELECT DISTINCT m.conversation_id
             FROM messages m
             JOIN conversations c ON c.id = m.conversation_id
             ORDER BY m.conversation_id ASC",
            &[] as &[ParamValue],
            |row| row.get_typed(0),
        )?;
        let envelopes = fetch_canonical_embedding_conversations(&storage, &conversation_ids)?;
        let mut grouped_messages =
            storage.fetch_messages_for_lexical_rebuild_batch(&conversation_ids, None, None)?;
        let mut packets: Vec<ConversationPacket> = Vec::with_capacity(envelopes.len());
        let mut contexts: Vec<SemanticPacketContext> = Vec::with_capacity(envelopes.len());
        for envelope in &envelopes {
            let messages = grouped_messages
                .remove(&envelope.conversation_id)
                .unwrap_or_default();
            let provenance = canonical_embedding_packet_provenance(envelope);
            let canonical = canonical_embedding_conversation(envelope, &provenance, messages);
            packets.push(ConversationPacket::from_canonical_replay(
                &canonical, provenance,
            ));
            contexts.push(SemanticPacketContext {
                conversation_id: envelope.conversation_id,
                agent_id: saturating_u32_from_i64(envelope.agent_id),
                workspace_id: saturating_u32_from_i64(envelope.workspace_id.unwrap_or(0)),
            });
        }
        let packet_inputs = semantic_inputs_from_packets(&packets, &contexts)?;

        // The two paths must produce the same EmbeddingInput list
        // (sortable comparison normalizes ordering across the two
        // helpers' iteration orders).
        assert!(
            !storage_inputs.is_empty(),
            "fixture should produce non-empty semantic inputs (sanity)"
        );
        assert_eq!(
            comparable_semantic_inputs(storage_inputs.clone()),
            comparable_semantic_inputs(packet_inputs.clone()),
            "packet-driven semantic preparation must match storage replay byte-for-byte"
        );

        // Sanity-pin a couple of contract details so a regression in
        // either path (e.g. role normalization or empty-content
        // filtering) trips a clear assertion rather than a generic
        // length mismatch.
        let storage_count = storage_inputs.len();
        let packet_count = packet_inputs.len();
        assert_eq!(
            storage_count, packet_count,
            "storage and packet semantic input counts must agree exactly"
        );
        // Empty-content system message must NOT appear in the output.
        assert!(
            packet_inputs.iter().all(|input| !input.content.is_empty()),
            "empty content must be filtered by the packet semantic projection"
        );
        // The remote-host source_id pins the cross-path provenance hash.
        let normalized_source_id =
            normalized_index_source_id(Some("remote-laptop"), None, Some("builder-host"));
        let expected_hash = crc32fast::hash(normalized_source_id.as_bytes());
        assert!(
            packet_inputs
                .iter()
                .all(|input| input.source_id == expected_hash),
            "every emitted EmbeddingInput must hash provenance via the packet's normalized source_id"
        );

        Ok(())
    }

    /// Length-mismatch defense: if a caller hands `semantic_inputs_from_packets`
    /// a packet/context slice pair of different lengths, the helper must
    /// return an error rather than silently mis-correlating ids. Pinning
    /// this is part of the bead's "shadow / compare mode plus explicit
    /// kill-switch" acceptance language.
    #[test]
    fn semantic_inputs_from_packets_rejects_length_mismatch() {
        let provenance = ConversationPacketProvenance::local();
        let canonical = test_conversation("packet-mismatch", "hello");
        let packet = ConversationPacket::from_canonical_replay(&canonical, provenance);
        let result = semantic_inputs_from_packets(&[packet], &[]);
        assert!(
            result.is_err(),
            "expected error on packet/context length mismatch"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("length mismatch"),
            "error should mention length mismatch, got: {err}"
        );
    }
}
