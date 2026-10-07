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
use frankensearch::quill::cass::CassDocument;
use serde::{Deserialize, Serialize};

use crate::model::types::Message;
use crate::search::asset_state::SearchMaintenanceMode;
use crate::search::tantivy::{TantivyIndex, expected_index_dir};
use crate::storage::sqlite::{FrankenStorage, LexicalRebuildConversationRow};

mod canary;
mod checkpoint;
mod snapshot;

const CHECKPOINT_MAX_BYTES: u64 = 64 * 1024;
const RECONCILE_BATCH_MAX_CONTENT_BYTES: usize = 64 * 1024 * 1024;
const RECONCILE_BATCH_MAX_MESSAGES: usize = 16_384;

#[derive(Debug, Default, PartialEq, Eq)]
struct ProjectionSummary {
    message_count: usize,
    max_message_idx: i64,
    content_bytes: usize,
    expected_docs: usize,
}

impl ProjectionSummary {
    fn observe_messages(&mut self, messages: &[Message]) -> Result<()> {
        for message in messages {
            if self.message_count == 0 || message.idx > self.max_message_idx {
                self.max_message_idx = message.idx;
            }
            self.message_count = self
                .message_count
                .checked_add(1)
                .ok_or_else(|| anyhow!("lexical reconcile message count overflow"))?;
            self.content_bytes = self
                .content_bytes
                .checked_add(message.content.len())
                .ok_or_else(|| anyhow!("lexical reconcile content byte count overflow"))?;
        }
        Ok(())
    }

    fn matches_checkpoint(&self, checkpoint: &LexicalReconcileCheckpoint) -> bool {
        self.message_count == checkpoint.message_count
            && self.max_message_idx == checkpoint.max_message_idx
            && self.content_bytes == checkpoint.content_bytes
            && self.expected_docs == checkpoint.expected_docs
    }
}

/// Re-read canonical rows instead of retaining an entire giant conversation,
/// its packet, and its projected documents through both index publications.
/// Only one batch is live. A message larger than the batch byte budget is
/// admitted alone, still subject to the storage layer's per-message text cap;
/// splitting or dropping that message here would change its lexical identity.
struct CanonicalProjection<'a> {
    storage: &'a FrankenStorage,
    row: &'a LexicalRebuildConversationRow,
    provenance: &'a super::LexicalRebuildPacketProvenance,
    conversation_id: i64,
    max_content_bytes: usize,
    max_messages: usize,
}

impl CanonicalProjection<'_> {
    fn visit(
        &self,
        mut visitor: impl FnMut(&[CassDocument]) -> Result<()>,
    ) -> Result<ProjectionSummary> {
        anyhow::ensure!(
            self.max_content_bytes > 0 && self.max_messages > 0,
            "lexical reconcile batch limits must be positive"
        );
        let mut summary = ProjectionSummary::default();
        let mut batch = Vec::new();
        let mut batch_bytes = 0usize;
        let mut flush = |messages: &mut Vec<Message>| -> Result<()> {
            if messages.is_empty() {
                return Ok(());
            }
            summary.observe_messages(messages)?;
            let packet = super::lexical_rebuild_contract_from_canonical_messages(
                self.row,
                self.provenance,
                std::mem::take(messages),
            );
            let docs = TantivyIndex::build_packet_documents(&packet, Some(self.conversation_id));
            drop(packet);
            summary.expected_docs = summary
                .expected_docs
                .checked_add(docs.len())
                .ok_or_else(|| anyhow!("lexical reconcile document count overflow"))?;
            visitor(&docs)
        };
        let completed = self.storage.for_each_lexical_rebuild_message(
            self.conversation_id,
            None,
            |message| {
                if !batch.is_empty()
                    && (batch.len() >= self.max_messages
                        || message.content.len()
                            > self.max_content_bytes.saturating_sub(batch_bytes))
                {
                    flush(&mut batch)?;
                    batch_bytes = 0;
                }
                batch_bytes = batch_bytes
                    .checked_add(message.content.len())
                    .ok_or_else(|| anyhow!("lexical reconcile batch byte count overflow"))?;
                batch.push(message);
                if batch_bytes >= self.max_content_bytes || batch.len() >= self.max_messages {
                    flush(&mut batch)?;
                    batch_bytes = 0;
                }
                Ok(true)
            },
        )?;
        anyhow::ensure!(
            completed,
            "lexical reconcile canonical stream stopped early"
        );
        flush(&mut batch)?;
        Ok(summary)
    }

    // Keep the original complete replay as an independent test comparator.
    // Production uses publication-bound durable progress below.
    #[cfg(test)]
    fn publish(
        &self,
        index: &mut TantivyIndex,
        checkpoint: &LexicalReconcileCheckpoint,
    ) -> Result<usize> {
        let mut fingerprint = checkpoint::ProjectionFingerprint::new(checkpoint.expected_docs);
        let mut upserted_docs = 0usize;
        let summary = self.visit(|docs| {
            fingerprint.update(docs)?;
            if !docs.is_empty() {
                upserted_docs = upserted_docs
                    .checked_add(index.upsert_prebuilt_documents_slice(docs)?)
                    .ok_or_else(|| anyhow!("lexical reconcile upsert count overflow"))?;
                // A bounded row reader alone is insufficient: committing each
                // batch also prevents the writer's pending set growing with
                // the full conversation. Recovery stays durable throughout.
                index.commit()?;
            }
            Ok(())
        })?;
        let digest = fingerprint.finish()?;
        anyhow::ensure!(
            summary.matches_checkpoint(checkpoint)
                && checkpoint.projection_blake3.as_deref() == Some(digest.as_str()),
            "canonical lexical projection changed during replay; checkpoint retained"
        );
        Ok(upserted_docs)
    }
}

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
    /// First-pass documents submitted in this invocation, excluding a resumed
    /// prefix. Zero is valid when resuming the verification replay pass.
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
    anyhow::ensure!(
        file.metadata()?.is_file(),
        "reconcile checkpoint is not a regular file"
    );
    let mut raw = Vec::new();
    file.take(CHECKPOINT_MAX_BYTES + 1).read_to_end(&mut raw)?;
    anyhow::ensure!(
        raw.len() as u64 <= CHECKPOINT_MAX_BYTES,
        "reconcile checkpoint exceeds its 64 KiB budget; checkpoint retained"
    );
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
    anyhow::ensure!(
        conversation_id > 0,
        "reconcile conversation id must be positive"
    );
    let _run_lock = super::acquire_index_run_lock(data_dir, db_path, SearchMaintenanceMode::Index)?;

    let storage = FrankenStorage::open_readonly(db_path)
        .with_context(|| format!("opening canonical archive {} read-only", db_path.display()))?;
    // The index-run lock serializes CASS writers, but it is not a database
    // snapshot. Bind metadata, the count/hash passes, and both bounded replays
    // to one view even if another application writes the archive meanwhile.
    let snapshot = snapshot::CanonicalSnapshot::begin(storage.raw())?;

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

    let source_map: HashMap<String, (crate::sources::provenance::SourceKind, Option<String>)> =
        storage
            .list_sources()
            .context("loading canonical source provenance for reconcile")?
            .into_iter()
            .map(|source| (source.id, (source.kind, source.host_label)))
            .collect();
    let (provenance, _mode) =
        super::lexical_rebuild_packet_provenance_from_canonical(&row, &source_map);
    let projection = CanonicalProjection {
        storage: &storage,
        row: &row,
        provenance: &provenance,
        conversation_id,
        max_content_bytes: super::responsiveness::effective_inflight_byte_limit(
            RECONCILE_BATCH_MAX_CONTENT_BYTES,
        ),
        max_messages: RECONCILE_BATCH_MAX_MESSAGES,
    };
    // Version two prefixes the exact projected document count. A bounded
    // count pass preserves that durable format, including noise filtering,
    // without retaining all the text just to learn its final count.
    let summary = projection.visit(|_| Ok(()))?;
    if summary.message_count == 0 {
        bail!("canonical conversation {conversation_id} has no messages to reconcile");
    }
    if summary.expected_docs == 0 {
        bail!(
            "conversation {conversation_id} projects to zero lexical documents \
             (all messages are filtered as noise); nothing to reconcile"
        );
    }
    let mut fingerprint = checkpoint::ProjectionFingerprint::new(summary.expected_docs);
    let mut early_doc = None;
    let mut late_doc = None;
    let bound_summary = projection.visit(|docs| {
        fingerprint.update(docs)?;
        if let Some(first) = docs.first() {
            if early_doc.is_none() {
                early_doc = Some(first.clone());
            }
            late_doc = docs.last().cloned();
        }
        Ok(())
    })?;
    anyhow::ensure!(
        summary == bound_summary,
        "canonical lexical projection changed during preflight; checkpoint retained"
    );
    let digest = fingerprint.finish()?;
    let early_doc = early_doc.ok_or_else(|| anyhow!("missing early reconcile endpoint"))?;
    let late_doc = late_doc.ok_or_else(|| anyhow!("missing late reconcile endpoint"))?;
    let early_token = canary_token(&early_doc.content);
    let late_token = canary_token(&late_doc.content);

    // 2. Durable checkpoint BEFORE publication; on retry, converge only when
    // the complete projected content and metadata are unchanged. A legacy
    // shape-only checkpoint is rebound before the full replay, never trusted
    // as evidence that any document was already published.
    let index_path = expected_index_dir(data_dir);
    let checkpoint_path = lexical_reconcile_checkpoint_path(&index_path, conversation_id);
    std::fs::create_dir_all(&index_path)
        .with_context(|| format!("creating index directory {}", index_path.display()))?;
    let checkpoint = checkpoint::resume(
        LexicalReconcileCheckpoint {
            version: checkpoint::VERSION,
            conversation_id,
            source_id: row.source_id.clone(),
            source_path: row.source_path.to_string_lossy().to_string(),
            message_count: summary.message_count,
            max_message_idx: summary.max_message_idx,
            content_bytes: summary.content_bytes,
            expected_docs: summary.expected_docs,
            projection_blake3: Some(digest),
            started_at_ms: FrankenStorage::now_millis(),
            attempt: 1,
        },
        load_checkpoint(&checkpoint_path)?,
    )?;
    let attempt = checkpoint.attempt;
    super::write_json_pretty_atomically(&checkpoint_path, &checkpoint)?;

    // 3. Upsert the full source doc set and publish a successor generation.
    let mut index = TantivyIndex::open_or_create(&index_path)?;
    let doc_count_before = index.doc_count()?;
    // 4. Both passes retain publication-bound cursors. A retry validates the
    // canonical prefix and resumes only writes justified by the same admitted
    // manifest; changed publication authority safely restarts a full replay.
    // This retains the existing count/endpoint invariant, not a full engine
    // content-witness audit of every document.
    let published = checkpoint::progress::publish(
        &projection,
        &mut index,
        &checkpoint,
        &index_path,
        &checkpoint_path,
    )?;
    let upserted_docs = published.upserted_docs;
    let doc_count_after_first = published.first_pass_live_docs;
    // Reuse one admitted reader for final accounting and both endpoint checks.
    // No refresh or path reopen may split these observations across generations.
    let reader = index.reader()?;
    let doc_count_after = reader.doc_count()?;
    let converged = doc_count_after.cmp(&doc_count_after_first).is_eq();

    // 5. Early/late endpoints against the published snapshot. Tokenless
    // messages must be verified too; unknown evidence cannot clear recovery.
    let early_canary_ok = canary::verify(&reader, &early_doc, early_token.as_deref())?;
    let late_canary_ok = canary::verify(&reader, &late_doc, late_token.as_deref())?;

    // Release explicitly before clearing recovery: a failed transaction
    // cleanup is not a successful repair. Early errors release through Drop.
    snapshot.release()?;

    let canaries_ok = early_canary_ok && late_canary_ok;
    let checkpoint_cleared = if converged && canaries_ok {
        // Remove the optional cursor first. A crash between these operations
        // can only cause an extra replay, never leave completion without proof.
        checkpoint::progress::clear(&checkpoint_path)?;
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
        message_count: summary.message_count,
        expected_docs: summary.expected_docs,
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
    use super::super::LexicalRebuildPacketProvenance;
    use super::*;
    use crate::model::conversation_packet::{ConversationPacket, ConversationPacketProvenance};
    use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
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

    fn stored_projection(
        tmp: &TempDir,
        messages: Vec<Message>,
    ) -> Result<(
        FrankenStorage,
        LexicalRebuildConversationRow,
        LexicalRebuildPacketProvenance,
    )> {
        let storage = FrankenStorage::open(&tmp.path().join("archive.db"))?;
        let agent_id = storage.ensure_agent(&Agent {
            id: None,
            slug: "codex".to_string(),
            name: "Codex".to_string(),
            version: None,
            kind: AgentKind::Cli,
        })?;
        let inserted = storage.insert_conversation_tree(agent_id, None, &conversation(messages))?;
        let (agents, workspaces) = storage.build_lexical_rebuild_lookups()?;
        let row = storage
            .list_conversations_for_lexical_rebuild_after_id(
                1,
                inserted.conversation_id - 1,
                &agents,
                &workspaces,
            )?
            .into_iter()
            .next()
            .context("missing test conversation")?;
        let (provenance, _) =
            super::super::lexical_rebuild_packet_provenance_from_canonical(&row, &HashMap::new());
        Ok((storage, row, provenance))
    }

    #[test]
    #[serial_test::serial]
    fn bounded_projection_preserves_sparse_indices_noise_and_oversized_messages() -> Result<()> {
        let tmp = TempDir::new()?;
        let (storage, row, provenance) = stored_projection(
            &tmp,
            vec![
                message(0, "alpha meridian content"),
                message(2, ""),
                message(7, "unicode λλ meridian content"),
                message(11, &"oversized meridian content ".repeat(30)),
                message(15, "bravo meridian content"),
                message(21, "final meridian content"),
            ],
        )?;
        let conversation_id = row.id.context("missing test identity")?;
        let messages = storage.fetch_messages_for_lexical_rebuild(conversation_id)?;
        let expected_bytes = messages
            .iter()
            .map(|message| message.content.len())
            .sum::<usize>();
        let packet = super::super::lexical_rebuild_contract_from_canonical_messages(
            &row,
            &provenance,
            messages,
        );
        let expected = TantivyIndex::build_packet_documents(&packet, Some(conversation_id));
        let projection = CanonicalProjection {
            storage: &storage,
            row: &row,
            provenance: &provenance,
            conversation_id,
            max_content_bytes: 64,
            max_messages: 2,
        };
        let mut observed = Vec::new();
        let mut batches = 0;
        let summary = projection.visit(|docs| {
            assert!(docs.len() <= 2, "row admission must stay bounded");
            let bytes = docs.iter().map(|doc| doc.content.len()).sum::<usize>();
            assert!(
                bytes <= 64 || docs.len() == 1,
                "oversized messages must be alone"
            );
            batches += usize::from(!docs.is_empty());
            observed.extend_from_slice(docs);
            Ok(())
        })?;
        assert!(
            batches >= 3,
            "fixture must cross real reader batch boundaries"
        );
        assert_eq!(summary.message_count, 6);
        assert_eq!(summary.max_message_idx, 21);
        assert_eq!(summary.content_bytes, expected_bytes);
        assert_eq!(summary.expected_docs, expected.len());
        assert_eq!(
            checkpoint::projection_fingerprint(&observed),
            checkpoint::projection_fingerprint(&expected),
            "chunking must preserve every projected byte and identity"
        );
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn bounded_projection_propagates_consumer_errors_without_poisoning_the_reader() -> Result<()> {
        let tmp = TempDir::new()?;
        let (storage, row, provenance) = stored_projection(
            &tmp,
            (0..5).map(|idx| message(idx, "meridian content")).collect(),
        )?;
        let projection = CanonicalProjection {
            storage: &storage,
            row: &row,
            provenance: &provenance,
            conversation_id: row.id.context("missing test identity")?,
            max_content_bytes: 32,
            max_messages: 1,
        };
        let mut visits = 0;
        let error = projection
            .visit(|_| {
                visits += 1;
                bail!("consumer failed")
            })
            .unwrap_err();
        assert!(error.to_string().contains("consumer failed"));
        assert_eq!(visits, 1);
        assert_eq!(projection.visit(|_| Ok(()))?.message_count, 5);
        let invalid = CanonicalProjection {
            max_content_bytes: 0,
            ..projection
        };
        assert!(
            invalid
                .visit(|_| panic!("invalid budget reached the consumer"))
                .is_err()
        );
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn bounded_publications_backfill_and_replay_without_losing_the_checkpoint() -> Result<()> {
        let tmp = TempDir::new()?;
        let (storage, row, provenance) = stored_projection(
            &tmp,
            (0..8)
                .map(|idx| message(idx, &format!("meridian endpoint {idx}")))
                .collect(),
        )?;
        let conversation_id = row.id.context("missing test identity")?;
        let projection = CanonicalProjection {
            storage: &storage,
            row: &row,
            provenance: &provenance,
            conversation_id,
            max_content_bytes: 64,
            max_messages: 2,
        };
        let mut docs = Vec::new();
        let summary = projection.visit(|batch| {
            docs.extend_from_slice(batch);
            Ok(())
        })?;
        assert_eq!(docs.len(), 8);
        let checkpoint = LexicalReconcileCheckpoint {
            version: checkpoint::VERSION,
            conversation_id,
            source_id: row.source_id.clone(),
            source_path: row.source_path.to_string_lossy().to_string(),
            message_count: summary.message_count,
            max_message_idx: summary.max_message_idx,
            content_bytes: summary.content_bytes,
            expected_docs: summary.expected_docs,
            projection_blake3: Some(checkpoint::projection_fingerprint(&docs)),
            started_at_ms: 1,
            attempt: 1,
        };
        let index_path = tmp.path().join("index");
        let mut index = TantivyIndex::open_or_create(&index_path)?;
        index.add_prebuilt_documents_slice(&docs[4..])?;
        index.commit()?;
        assert_eq!(index.doc_count()?, 4);
        let path = lexical_reconcile_checkpoint_path(&index_path, conversation_id);
        super::super::write_json_pretty_atomically(&path, &checkpoint)?;
        let original = std::fs::read(&path)?;
        for _ in 0..2 {
            projection.publish(&mut index, &checkpoint)?;
            assert_eq!(index.doc_count()?, 8);
            assert_eq!(
                std::fs::read(&path)?,
                original,
                "only final verification may clear recovery"
            );
        }
        let reader = index.reader()?;
        for doc in &docs {
            assert!(canary::verify(
                &reader,
                doc,
                canary_token(&doc.content).as_deref()
            )?);
        }
        Ok(())
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
        assert!(load_checkpoint(&path)?.is_none());
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
