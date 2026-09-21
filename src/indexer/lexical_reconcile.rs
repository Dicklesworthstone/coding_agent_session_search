//! Targeted, idempotent lexical reconcile for ONE canonical conversation
//! (bead qhiv2, gh#382 partial-prefix recovery).
//!
//! Incremental replay indexes only `InsertOutcome.inserted_indices`, so an
//! interrupted giant conversation can keep canonical rows with no lexical
//! docs — and replay never backfills the prefix. This module implements the
//! maintainer-accepted five-step operation, source-scoped and retry-safe,
//! with no corpus-wide replay:
//!
//! 1. bind one canonical conversation/source identity plus an immutable
//!    content-bound fingerprint of every projected lexical document;
//! 2. persist a durable recovery checkpoint with the expected doc count
//!    BEFORE any publication;
//! 3. upsert the full source doc set under Quill's stable CASS document
//!    identities (source id + source path + conversation id + msg idx),
//!    then publish a successor generation;
//! 4. on retry, re-read the checkpoint and converge to exactly one live doc
//!    per identity (upsert replaces; never appends);
//! 5. verify exact endpoint canaries and the replay live-doc count before clearing the
//!    durable checkpoint.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::search::asset_state::SearchMaintenanceMode;
use crate::search::tantivy::{TantivyIndex, expected_index_dir};
use crate::storage::sqlite::FrankenStorage;

mod checkpoint;
mod canary;

const CHECKPOINT_MAX_BYTES: u64 = 64 * 1024;

/// Durable recovery checkpoint written before the first publication and
/// cleared only after convergence + canary verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LexicalReconcileCheckpoint {
    pub version: u32,
    pub conversation_id: i64,
    pub source_id: String,
    pub source_path: String,
    /// Shape diagnostics, not proof that the underlying content is unchanged.
    pub message_count: usize,
    pub max_message_idx: i64,
    pub content_bytes: usize,
    /// Lexical docs the bound source set projects to (post noise filter).
    pub expected_docs: usize,
    /// Version two binds all projected content/metadata. Absent only in v1.
    #[serde(default)]
    pub projection_blake3: Option<String>,
    pub started_at_ms: i64,
    pub attempt: u32,
}

/// Machine-readable outcome of one reconcile run.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct LexicalReconcileReport {
    pub conversation_id: i64,
    pub source_id: String,
    pub source_path: String,
    pub attempt: u32,
    pub message_count: usize,
    pub expected_docs: usize,
    pub upserted_docs: usize,
    pub doc_count_before: u64,
    pub doc_count_after: u64,
    /// True when a second upsert of the identical set left the live-doc count
    /// unchanged. This is a replay invariant, not a full content-witness audit.
    pub converged: bool,
    /// Early/late endpoints observed at the exact source/message identity with
    /// matching stored previews. New runs always emit Some(bool), using bounded
    /// keyword discovery when no text token is available. The optional shape
    /// is retained for compatibility with older reports, not a success bypass.
    pub early_canary_ok: Option<bool>,
    pub late_canary_ok: Option<bool>,
    pub checkpoint_cleared: bool,
}

pub(crate) fn lexical_reconcile_checkpoint_path(
    index_path: &Path,
    conversation_id: i64,
) -> PathBuf {
    index_path.join(format!(".lexical-reconcile-{conversation_id}.json"))
}

fn load_checkpoint(path: &Path) -> Result<Option<LexicalReconcileCheckpoint>> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) if matches!(err.kind(), std::io::ErrorKind::NotFound) => return Ok(None),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("reading reconcile checkpoint {}", path.display()));
        }
    };
    anyhow::ensure!(file.metadata()?.is_file(), "reconcile checkpoint is not a regular file");
    let mut raw = Vec::new();
    file.take(CHECKPOINT_MAX_BYTES + 1).read_to_end(&mut raw)?;
    anyhow::ensure!(raw.len() as u64 <= CHECKPOINT_MAX_BYTES,
        "reconcile checkpoint exceeds its 64 KiB budget; checkpoint retained");
    serde_json::from_slice(&raw)
        .map(Some)
        .with_context(|| format!("parsing reconcile checkpoint {}", path.display()))
}

fn clear_checkpoint(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if matches!(err.kind(), std::io::ErrorKind::NotFound) => Ok(()),
        Err(err) => {
            Err(err).with_context(|| format!("clearing reconcile checkpoint {}", path.display()))
        }
    }
}

/// First lowercase alphanumeric token (>= 4 chars) usable as a search canary.
fn canary_token(content: &str) -> Option<String> {
    content
        .split(|c: char| !c.is_alphanumeric())
        .find(|word| word.chars().count() >= 4 && word.chars().any(|c| c.is_alphabetic()))
        .map(str::to_lowercase)
}

/// Run the targeted reconcile for one canonical conversation.
///
/// Holds the index-run lock for the whole operation (never races the
/// indexer), opens the canonical archive READ-ONLY, and touches only the
/// derived lexical index plus its own checkpoint sidecar.
pub(crate) fn run_lexical_conversation_reconcile(
    data_dir: &Path,
    db_path: &Path,
    conversation_id: i64,
) -> Result<LexicalReconcileReport> {
    anyhow::ensure!(conversation_id > 0, "reconcile conversation id must be positive");
    let _run_lock = super::acquire_index_run_lock(data_dir, db_path, SearchMaintenanceMode::Index)?;

    let storage = FrankenStorage::open_readonly(db_path)
        .with_context(|| format!("opening canonical archive {} read-only", db_path.display()))?;

    // 1. Bind the conversation identity.
    let (agent_slugs, workspace_paths) = storage
        .build_lexical_rebuild_lookups()
        .context("loading agent/workspace lookups for reconcile")?;
    let row = storage
        .list_conversations_for_lexical_rebuild_after_id(
            1,
            conversation_id.saturating_sub(1),
            &agent_slugs,
            &workspace_paths,
        )?
        .into_iter()
        .find(|row| row.id.is_some_and(|id| id.cmp(&conversation_id).is_eq()))
        .ok_or_else(|| {
            anyhow!("canonical conversation {conversation_id} not found in the archive")
        })?;

    // Capped message rows in idx order — the immutable source set this run
    // binds. The cap matches every other lexical path, so the projection is
    // byte-identical to what a healthy inline index would have produced.
    let messages = storage.fetch_messages_for_lexical_rebuild(conversation_id)?;
    if messages.is_empty() {
        bail!("canonical conversation {conversation_id} has no messages to reconcile");
    }
    let message_count = messages.len();
    let max_message_idx = messages.iter().map(|m| m.idx).max().unwrap_or(0);
    let content_bytes: usize = messages.iter().map(|m| m.content.len()).sum();

    let source_map: HashMap<String, (crate::sources::provenance::SourceKind, Option<String>)> =
        storage
            .list_sources()
            .context("loading canonical source provenance for reconcile")?
            .into_iter()
            .map(|source| (source.id, (source.kind, source.host_label)))
            .collect();
    let (provenance, _mode) =
        super::lexical_rebuild_packet_provenance_from_canonical(&row, &source_map);
    let packet =
        super::lexical_rebuild_contract_from_canonical_messages(&row, &provenance, messages);

    let index_path = expected_index_dir(data_dir);
    let docs = TantivyIndex::build_packet_documents(&packet, Some(conversation_id));
    if docs.is_empty() {
        bail!(
            "conversation {conversation_id} projects to zero lexical documents \
             (all messages are filtered as noise); nothing to reconcile"
        );
    }
    let early_token = docs.first().and_then(|doc| canary_token(&doc.content));
    let late_token = docs.last().and_then(|doc| canary_token(&doc.content));

    // 2. Durable checkpoint BEFORE publication; on retry, converge only when
    // the complete projected content and metadata are unchanged. A legacy
    // shape-only checkpoint is rebound before the full replay, never trusted
    // as evidence that any document was already published.
    let checkpoint_path = lexical_reconcile_checkpoint_path(&index_path, conversation_id);
    std::fs::create_dir_all(&index_path)
        .with_context(|| format!("creating index directory {}", index_path.display()))?;
    let checkpoint = checkpoint::resume(LexicalReconcileCheckpoint {
        version: checkpoint::VERSION,
        conversation_id,
        source_id: row.source_id.clone(),
        source_path: row.source_path.to_string_lossy().to_string(),
        message_count,
        max_message_idx,
        content_bytes,
        expected_docs: docs.len(),
        projection_blake3: Some(checkpoint::projection_fingerprint(&docs)),
        started_at_ms: FrankenStorage::now_millis(),
        attempt: 1,
    }, load_checkpoint(&checkpoint_path)?)?;
    let attempt = checkpoint.attempt;
    super::write_json_pretty_atomically(&checkpoint_path, &checkpoint)?;

    // 3. Upsert the full source doc set and publish a successor generation.
    let mut index = TantivyIndex::open_or_create(&index_path)?;
    let doc_count_before = index.doc_count()?;
    let upserted_docs = index.upsert_prebuilt_documents_slice(&docs)?;
    index.commit()?;
    let doc_count_after_first = index.doc_count()?;

    // 4. Preserve the existing replay invariant until the CASS adapter exposes
    // Quill's writer-side per-document witnesses. Counts alone do not prove
    // that every expected projected document is present with matching content.
    index.upsert_prebuilt_documents_slice(&docs)?;
    index.commit()?;
    // Reuse one admitted reader for final accounting and both endpoint checks.
    // No refresh or path reopen may split these observations across generations.
    let reader = index.reader()?;
    let doc_count_after = reader.doc_count()?;
    let converged = doc_count_after.cmp(&doc_count_after_first).is_eq();

    // 5. Early/late endpoints against the published snapshot. Tokenless
    // messages must be verified too; unknown evidence cannot clear recovery.
    let early_canary_ok = canary::verify(&reader, &docs[0], early_token.as_deref())?;
    let late_canary_ok = canary::verify(&reader, &docs[docs.len() - 1], late_token.as_deref())?;

    let canaries_ok = early_canary_ok && late_canary_ok;
    let checkpoint_cleared = if converged && canaries_ok {
        clear_checkpoint(&checkpoint_path)?;
        true
    } else {
        false
    };

    let report = LexicalReconcileReport {
        conversation_id,
        source_id: checkpoint.source_id,
        source_path: checkpoint.source_path,
        attempt,
        message_count,
        expected_docs: docs.len(),
        upserted_docs,
        doc_count_before,
        doc_count_after,
        converged,
        early_canary_ok: Some(early_canary_ok),
        late_canary_ok: Some(late_canary_ok),
        checkpoint_cleared,
    };
    if !checkpoint_cleared {
        bail!(
            "reconcile of conversation {conversation_id} did not verify \
             (converged: {converged}, early canary: {early_canary_ok:?}, late canary: \
             {late_canary_ok:?}); the durable checkpoint was retained — rerun to retry: {}",
            serde_json::to_string(&report).unwrap_or_default()
        );
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::conversation_packet::{ConversationPacket, ConversationPacketProvenance};
    use crate::model::types::{Conversation, Message, MessageRole};
    use tempfile::TempDir;

    fn message(idx: i64, content: &str) -> Message {
        Message {
            id: Some(idx + 1),
            idx,
            role: MessageRole::User,
            author: None,
            created_at: Some(1_700_000_000_000 + idx),
            content: content.to_string(),
            extra_json: serde_json::Value::Null,
            snippets: Vec::new(),
        }
    }

    fn conversation(messages: Vec<Message>) -> Conversation {
        Conversation {
            id: Some(42),
            agent_slug: "codex".to_string(),
            workspace: None,
            external_id: Some("reconcile-conv".to_string()),
            title: Some("reconcile test".to_string()),
            source_path: PathBuf::from("/tmp/reconcile-src.jsonl"),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_009_000),
            approx_tokens: None,
            metadata_json: serde_json::Value::Null,
            messages,
            source_id: "local".to_string(),
            origin_host: None,
        }
    }

    /// The core converge property at the index layer: a partial-prefix index
    /// (half the docs added) reaches the full doc set through upsert, and a
    /// second identical upsert leaves the live count unchanged.
    #[test]
    fn upsert_backfills_partial_prefix_and_converges() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let index_path = tmp.path().join("index");
        let conv = conversation(
            (0..10)
                .map(|i| message(i, &format!("reconcile marker alpha{i} bravo{i}")))
                .collect(),
        );
        let packet = ConversationPacket::from_canonical_replay(
            &conv,
            ConversationPacketProvenance {
                source_id: "local".to_string(),
                origin_kind: "local".to_string(),
                origin_host: None,
            },
        );
        let docs = TantivyIndex::build_packet_documents(&packet, Some(42));
        assert_eq!(docs.len(), 10);

        // Simulate the interrupted state: only the SUFFIX was indexed.
        let mut index = TantivyIndex::open_or_create(&index_path)?;
        index.add_prebuilt_documents_slice(&docs[5..])?;
        index.commit()?;
        assert_eq!(index.doc_count()?, 5);

        // Reconcile: upsert the full set — prefix backfilled, suffix replaced
        // in place (no duplicates).
        index.upsert_prebuilt_documents_slice(&docs)?;
        index.commit()?;
        assert_eq!(index.doc_count()?, 10, "prefix must be backfilled");

        // Retry converges: same set, same live count.
        index.upsert_prebuilt_documents_slice(&docs)?;
        index.commit()?;
        assert_eq!(index.doc_count()?, 10, "retry must never append");

        // Canaries: early and late markers resolve to this conversation.
        let early = canary_token(&docs[0].content);
        let late = canary_token(&docs[9].content);
        let reader = index.reader()?;
        assert!(canary::verify(&reader, &docs[0], early.as_deref())?);
        assert!(canary::verify(&reader, &docs[9], late.as_deref())?);
        // A wrong conversation id must not satisfy the canary.
        let mut wrong = docs[0].clone();
        wrong.conversation_id = Some(43);
        assert!(!canary::verify(&reader, &wrong, early.as_deref())?);
        Ok(())
    }

    #[test]
    fn checkpoint_roundtrip_and_paths() -> anyhow::Result<()> {
        let tmp = TempDir::new()?;
        let index_path = tmp.path().join("index");
        std::fs::create_dir_all(&index_path)?;
        let path = lexical_reconcile_checkpoint_path(&index_path, 42);
        assert!(load_checkpoint(&path)?.is_none());

        let checkpoint = LexicalReconcileCheckpoint {
            version: 1,
            conversation_id: 42,
            source_id: "local".to_string(),
            source_path: "/tmp/reconcile-src.jsonl".to_string(),
            message_count: 10,
            max_message_idx: 9,
            content_bytes: 320,
            expected_docs: 10,
            projection_blake3: None,
            started_at_ms: 1,
            attempt: 1,
        };
        crate::indexer::write_json_pretty_atomically(&path, &checkpoint)?;
        assert_eq!(load_checkpoint(&path)?, Some(checkpoint));
        clear_checkpoint(&path)?;
        assert!(load_checkpoint(&path)?.is_none());
        // Clearing an absent checkpoint stays Ok (idempotent retry surface).
        clear_checkpoint(&path)?;
        Ok(())
    }

    #[test]
    fn canary_token_prefers_meaningful_words() {
        assert_eq!(canary_token("a bb ccc dddd"), Some("dddd".to_string()));
        assert_eq!(
            canary_token("[Tool: execute] cargo test"),
            Some("tool".to_string())
        );
        assert_eq!(canary_token("1234 !!"), None);
        assert_eq!(canary_token(""), None);
    }
}
