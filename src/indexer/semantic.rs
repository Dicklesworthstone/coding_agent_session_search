use std::collections::HashSet;
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

use crate::indexer::memoization::{
    ContentAddressedMemoCache, MemoCacheAuditRecord, MemoContentHash, MemoKey, MemoLookup,
};
use crate::indexer::responsiveness;
use crate::model::conversation_packet::{ConversationPacket, ConversationPacketProvenance};
use crate::model::types::{Conversation, Message};
use crate::search::canonicalize::{canonicalize_for_embedding, content_hash};
use crate::search::embedder::Embedder;
use crate::search::fastembed_embedder::FastEmbedder;
use crate::search::hash_embedder::HashEmbedder;
use crate::search::policy::{CHUNKING_STRATEGY_VERSION, SEMANTIC_SCHEMA_VERSION, SemanticPolicy};
use crate::search::semantic_manifest::{
    ArtifactRecord, BuildCheckpoint, SemanticManifest, TierKind,
};
use crate::search::tantivy::{
    normalized_index_origin_host, normalized_index_origin_kind, normalized_index_source_id,
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
const DEFAULT_SEMANTIC_PREP_MEMO_CAPACITY: usize = 4_096;
const SEMANTIC_PREP_MEMO_ALGORITHM: &str = "semantic_prepare_window";
const SEMANTIC_PREP_MEMO_VERSION: &str = "canonicalize_for_embedding:v2:stable-content-hash";

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
}

pub(crate) struct CanonicalIncrementalEmbeddingBatch {
    pub inputs: Vec<EmbeddingInput>,
    pub conversations_in_batch: u64,
    pub raw_max_message_id: Option<i64>,
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
/// [`packet_embedding_inputs_from_storage`]: derives the same
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

    let conversations = fetch_canonical_embedding_conversations(storage, &conversation_ids)?;

    let mut grouped_messages =
        storage.fetch_messages_for_lexical_rebuild_batch(&conversation_ids, None, None)?;
    let mut inputs = Vec::new();
    for conversation in &conversations {
        let messages = grouped_messages
            .remove(&conversation.conversation_id)
            .unwrap_or_default();
        let provenance = canonical_embedding_packet_provenance(conversation);
        let canonical = canonical_embedding_conversation(conversation, &provenance, messages);
        let packet = ConversationPacket::from_canonical_replay(&canonical, provenance);
        inputs.extend(embedding_inputs_from_conversation_packet(
            conversation,
            &packet,
        ));
    }

    let conversations_in_batch = u64::try_from(conversation_ids.len()).unwrap_or(u64::MAX);
    tracing::debug!(
        conversations_in_batch,
        packet_driven = true,
        semantic_inputs = inputs.len(),
        "built semantic backfill batch from ConversationPacket canonical replay"
    );

    Ok(CanonicalEmbeddingBatch {
        inputs,
        conversations_in_batch,
        last_conversation_id: *conversation_ids.last().unwrap_or(&after_conversation_id),
        total_conversations,
    })
}

pub(crate) fn packet_embedding_inputs_from_storage(
    storage: &FrankenStorage,
) -> Result<Vec<EmbeddingInput>> {
    Ok(fetch_canonical_embedding_batch(storage, 0, usize::MAX)?.inputs)
}

fn packet_embedding_inputs_from_selected_canonical_messages<F>(
    storage: &FrankenStorage,
    conversation_ids: &[i64],
    mut include_message: F,
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
    let mut inputs = Vec::new();
    let mut raw_max_message_id: Option<i64> = None;

    for conversation in &conversations {
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

    Ok((inputs, raw_max_message_id))
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
            "fastembed" | "minilm" | "snowflake-arctic-s" | "nomic-embed" => {
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
        let serial_prep = !parallel_prep_enabled();
        let prep_memo_capacity = resolved_semantic_prep_memo_capacity();
        let mut prep_memo =
            serial_prep.then(|| ContentAddressedMemoCache::with_capacity(prep_memo_capacity));
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
                pb.inc(skipped_in_window as u64);
            }

            for batch in prepared_window.chunks(self.batch_size) {
                flush_prepared_batch(batch, &mut embeddings, &pb, self.embedder.as_ref())?;
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
        let mut staged_index = self.write_backfill_staging_index(
            embeddings,
            &staging_path,
            prior_checkpoint.is_some(),
        )?;
        let conversations_processed = prior_conversations
            .saturating_add(plan.conversations_in_batch)
            .min(plan.total_conversations);
        let complete = conversations_processed >= plan.total_conversations;

        manifest.refresh_backlog(plan.total_conversations, &plan.db_fingerprint);

        if complete {
            let db_fingerprint = plan.db_fingerprint.clone();
            if staged_index.wal_record_count() > 0 {
                staged_index.compact().map_err(|err| {
                    anyhow::anyhow!("compact staged semantic index failed: {err}")
                })?;
            }
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
            let docs_embedded_on_disk =
                u64::try_from(staged_index.record_count()).unwrap_or(u64::MAX);
            let checkpoint_docs = prior_docs
                .saturating_add(embedded_docs)
                .max(docs_embedded_on_disk);
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
    use std::path::Path;
    use tempfile::tempdir;

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

    fn test_conversation_with_messages(external_id: &str, messages: Vec<Message>) -> Conversation {
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
            source_id: "remote-laptop".to_string(),
            origin_host: Some("builder-host".to_string()),
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

    /// `coding_agent_session_search-ibuuh.32` (sink #3 equivalence gate):
    /// the packet-driven `semantic_inputs_from_packets` helper must
    /// produce the same `EmbeddingInput` list a fresh storage replay
    /// returns for the same canonical corpus. Once this passes, callers
    /// that already hold packets (rebuild pipeline, salvage replay,
    /// repair flows) can drive the semantic preparation consumer
    /// without a second canonical-row round-trip.
    #[test]
    fn semantic_inputs_from_packets_matches_storage_replay() -> Result<()> {
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
        let storage_inputs = packet_embedding_inputs_from_storage(&storage)?;

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
