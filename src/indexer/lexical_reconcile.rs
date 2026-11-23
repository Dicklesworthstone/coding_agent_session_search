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
//! 5. verify every projected document's native content witness, exact endpoint
//!    canaries and the replay live-doc count before clearing the checkpoint.

use std::collections::{BTreeSet, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use frankensearch::quill::cass::legacy_v9::cass_document_identity;
use frankensearch::quill::cass::{CassConversationKey, CassDocument, CassDocumentRef};
use serde::{Deserialize, Serialize};

use crate::model::types::Message;
use crate::search::asset_state::SearchMaintenanceMode;
use crate::search::tantivy::{TantivyIndex, expected_index_dir};
use crate::sources::provenance::LOCAL_SOURCE_ID;
use crate::storage::sqlite::{FrankenStorage, LexicalRebuildConversationRow};

pub(super) mod canary;
mod checkpoint;
pub(super) mod snapshot;

const CHECKPOINT_MAX_BYTES: u64 = 64 * 1024;
const RECONCILE_BATCH_MAX_CONTENT_BYTES: usize = 64 * 1024 * 1024;
const RECONCILE_BATCH_MAX_MESSAGES: usize = 16_384;
const LEGACY_CLEANUP_MAX_IDENTITIES: usize = 256;
const LEGACY_CLEANUP_MAX_ID_BYTES: usize = 64 * 1024;

fn legacy_local_identity_max_bytes(document: &CassDocument) -> Result<usize> {
    // The v9 key contains three separators and at most twenty decimal bytes
    // each for a signed conversation ID and an unsigned message index. Allow
    // for geometric String growth when splitting; actual retained capacities
    // are checked separately before any batch mutation.
    document
        .source_path
        .len()
        .checked_add(LOCAL_SOURCE_ID.len() + 3 + 20 + 20)
        .and_then(|bytes| bytes.checked_mul(2))
        .context("legacy lexical identity size overflow; checkpoint retained")
}

fn remove_legacy_local_copy_batch(
    index: &mut TantivyIndex,
    docs: &[CassDocument],
) -> Result<usize> {
    let identity_bytes = docs.iter().try_fold(0usize, |bytes, document| {
        bytes
            .checked_add(legacy_local_identity_max_bytes(document)?)
            .context("legacy lexical cleanup size overflow; checkpoint retained")
    })?;
    anyhow::ensure!(
        docs.len() <= LEGACY_CLEANUP_MAX_IDENTITIES
            && identity_bytes <= LEGACY_CLEANUP_MAX_ID_BYTES,
        "legacy lexical cleanup exceeds its identity budget; checkpoint retained"
    );
    if docs.is_empty() {
        return Ok(0);
    }
    let published = canary::exact::PublishedSnapshot::from_keeper(index.committed_snapshot()?)?;
    let count_before = published.doc_count();
    let mut identities = BTreeSet::new();
    let mut retained_capacity = 0usize;
    for document in docs {
        // An authoritative local identity must never be its own candidate,
        // even if its origin metadata is remote.
        if document.source_id == LOCAL_SOURCE_ID {
            continue;
        }
        let legacy = CassDocumentRef {
            source_id: LOCAL_SOURCE_ID,
            origin_kind: "local",
            origin_host: None,
            ..document.as_ref()
        };
        if published.classify_content_ref(legacy)? != canary::exact::ContentMatch::Matching {
            continue;
        }
        anyhow::ensure!(
            published.verify_content(document)?,
            "authoritative lexical counterpart is missing or stale for conversation {:?} \
             message {}; checkpoint retained",
            document.conversation_id,
            document.msg_idx
        );
        let identity = cass_document_identity(
            legacy.source_id,
            CassConversationKey::for_document(legacy),
            legacy.msg_idx,
        );
        retained_capacity = retained_capacity
            .checked_add(identity.capacity())
            .context("legacy lexical identity capacity overflow; checkpoint retained")?;
        anyhow::ensure!(
            retained_capacity <= LEGACY_CLEANUP_MAX_ID_BYTES,
            "legacy lexical cleanup exceeds its retained identity budget; checkpoint retained"
        );
        anyhow::ensure!(
            identities.insert(identity),
            "legacy lexical cleanup contains a repeated identity; checkpoint retained"
        );
    }
    // No previous generation remains pinned across a deletion. The writer
    // admission and run lock preserve the authority used above; Quill resolves
    // the same representative again and tombstones that one physical row.
    // Native ambiguity errors propagate. This is not a corpus-wide duplicate
    // audit or a repair of corrupted non-representative rows/tombstones.
    drop(published);
    if identities.is_empty() {
        return Ok(0);
    }
    let identities = identities.into_iter().collect::<Vec<_>>();
    let expected_count = count_before
        .checked_sub(u64::try_from(identities.len())?)
        .context("legacy lexical cleanup count underflow; checkpoint retained")?;
    let deleted = index.delete_witnessed_document_ids(&identities)?;
    anyhow::ensure!(
        deleted == identities.len() && index.doc_count()? == expected_count,
        "legacy lexical cleanup removed an unexpected document count; checkpoint retained"
    );
    Ok(identities.len())
}

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

    /// Audit every expected identity on ONE immutable published view while
    /// re-reading the caller-pinned canonical snapshot in bounded batches.
    /// Neither a stable global count nor two correct endpoints establishes
    /// that an interior message exists with all of its projected content.
    fn verify_published(
        &self,
        published: &canary::exact::PublishedSnapshot,
        checkpoint: &LexicalReconcileCheckpoint,
    ) -> Result<usize> {
        anyhow::ensure!(
            checkpoint.version == checkpoint::VERSION
                && checkpoint.expected_docs > 0
                && self.conversation_id == checkpoint.conversation_id
                && self.row.source_id == checkpoint.source_id
                && self.row.source_path.to_string_lossy().as_ref() == checkpoint.source_path,
            "invalid lexical verification binding; checkpoint retained"
        );
        let mut fingerprint = checkpoint::ProjectionFingerprint::new(checkpoint.expected_docs);
        let mut verified_docs = 0usize;
        let summary = self.visit(|docs| {
            fingerprint.update(docs)?;
            for document in docs {
                anyhow::ensure!(
                    published.verify_content(document)?,
                    "lexical repair content witness mismatch for conversation {} message {}; \
                     checkpoint retained",
                    self.conversation_id,
                    document.msg_idx
                );
            }
            verified_docs = verified_docs
                .checked_add(docs.len())
                .ok_or_else(|| anyhow!("lexical verification document count overflow"))?;
            tracing::debug!(
                conversation_id = self.conversation_id,
                verified_docs,
                expected_docs = checkpoint.expected_docs,
                "lexical repair content audit progressed"
            );
            Ok(())
        })?;
        let digest = fingerprint.finish()?;
        anyhow::ensure!(
            summary.matches_checkpoint(checkpoint)
                && checkpoint.projection_blake3.as_deref() == Some(digest.as_str()),
            "canonical lexical projection changed during verification; checkpoint retained"
        );
        Ok(verified_docs)
    }

    /// Remove only the exact local copies produced by the former canonical
    /// replay provenance bug. A different payload, metadata, or v9 identity
    /// collision is ambiguous evidence and must remain untouched. Deletion
    /// targets only the witnessed native representative; resolver ambiguity
    /// errors refuse the batch rather than authorize deleting other rows.
    fn remove_legacy_local_copies(
        &self,
        index: &mut TantivyIndex,
        checkpoint: &LexicalReconcileCheckpoint,
    ) -> Result<usize> {
        anyhow::ensure!(
            checkpoint.version == checkpoint::VERSION
                && checkpoint.expected_docs > 0
                && self.conversation_id == checkpoint.conversation_id
                && self.row.source_id == checkpoint.source_id
                && self.row.source_path.to_string_lossy().as_ref() == checkpoint.source_path,
            "invalid lexical cleanup binding; checkpoint retained"
        );
        if self.provenance.source_id == LOCAL_SOURCE_ID {
            return Ok(0);
        }
        let mut fingerprint = checkpoint::ProjectionFingerprint::new(checkpoint.expected_docs);
        let mut removed = 0usize;
        let summary = self.visit(|docs| {
            fingerprint.update(docs)?;
            let mut start = 0;
            while start < docs.len() {
                let mut end = start;
                let mut bytes = 0usize;
                while end < docs.len() && end - start < LEGACY_CLEANUP_MAX_IDENTITIES {
                    let next_bytes = legacy_local_identity_max_bytes(&docs[end])?;
                    anyhow::ensure!(
                        next_bytes <= LEGACY_CLEANUP_MAX_ID_BYTES,
                        "legacy lexical identity exceeds its cleanup budget; checkpoint retained"
                    );
                    if next_bytes > LEGACY_CLEANUP_MAX_ID_BYTES - bytes {
                        break;
                    }
                    bytes += next_bytes;
                    end += 1;
                }
                removed = removed
                    .checked_add(remove_legacy_local_copy_batch(index, &docs[start..end])?)
                    .context("legacy lexical cleanup count overflow; checkpoint retained")?;
                start = end;
            }
            Ok(())
        })?;
        anyhow::ensure!(
            summary.matches_checkpoint(checkpoint)
                && checkpoint.projection_blake3.as_deref() == Some(fingerprint.finish()?.as_str()),
            "canonical lexical projection changed during cleanup; checkpoint retained"
        );
        Ok(removed)
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
    /// Exact copies left under the old local provenance identity, removed
    /// only after their authoritative counterparts were fully witnessed.
    pub removed_legacy_local_copies: usize,
    /// Every expected identity checked against native full-document witnesses
    /// and stored columns on one immutable publication, without ranking.
    pub verified_docs: usize,
    /// Names the non-cryptographic engine witness, not a posting-level audit.
    pub verification_kind: &'static str,
    pub doc_count_before: u64,
    pub doc_count_after: u64,
    /// True when a second upsert of the identical set left the live-doc count
    /// unchanged. This is a replay invariant, not a full content-witness audit.
    pub converged: bool,
    /// Early/late endpoints observed at the exact source/message identity with
    /// matching stored previews. New runs always emit Some(bool), resolving
    /// native document identities even without a text token. The optional shape
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
#[cfg(test)]
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
    // Writable admission: the archive is opened through a recovery-capable
    // read-only open, which may repair it.
    let _run_lock = super::acquire_index_run_lock_with_job_kind(
        data_dir,
        db_path,
        SearchMaintenanceMode::Index,
        super::maintenance_job_kind_for_mode(SearchMaintenanceMode::Index),
    )?;

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

    // 2. Durable checkpoint BEFORE publication. Retry keeps an identical
    // binding or proves the old projected prefix unchanged before rebinding
    // appended rows. Legacy shape-only checkpoints require an unchanged shape
    // and full replay; they cannot prove an append-only extension.
    let index_path = expected_index_dir(data_dir);
    let checkpoint_path = lexical_reconcile_checkpoint_path(&index_path, conversation_id);
    std::fs::create_dir_all(&index_path)
        .with_context(|| format!("creating index directory {}", index_path.display()))?;
    let checkpoint = checkpoint::resume_on_snapshot(
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
        &projection,
    )?;
    let attempt = checkpoint.attempt;
    super::write_json_pretty_atomically(&checkpoint_path, &checkpoint)?;

    // 3. Upsert the full source doc set and publish a successor generation.
    let mut index = TantivyIndex::open_or_create(&index_path)?;
    let doc_count_before = index.doc_count()?;
    // 4. Both passes retain publication-bound cursors. A retry validates the
    // canonical prefix and resumes only writes justified by the same admitted
    // manifest; changed publication authority safely restarts a full replay.
    // Keep the existing replay invariant as well as the read-only content
    // audit below. A saved publication cursor is not a verification receipt.
    let published = checkpoint::progress::publish(
        &projection,
        &mut index,
        &checkpoint,
        &index_path,
        &checkpoint_path,
    )?;
    let upserted_docs = published.upserted_docs;
    let doc_count_after_first = published.first_pass_live_docs;
    let doc_count_after_replay = index.doc_count()?;
    let converged = doc_count_after_replay == doc_count_after_first;
    // Publication cursors are retained through cleanup. Each deletion changes
    // the manifest, so an interrupted cleanup invalidates its earlier cursor
    // and safely replays on retry. Never migrate incompatible old checkpoints.
    let removed_legacy_local_copies =
        projection.remove_legacy_local_copies(&mut index, &checkpoint)?;
    // One native admission supplies identity lookup, stored columns and live
    // accounting. Never rank endpoint discovery: an exact message can fall
    // below every page of a bounded common-term query on a large archive.
    let published = canary::exact::PublishedSnapshot::open(&index_path)?;
    let doc_count_after = published.doc_count();
    anyhow::ensure!(
        doc_count_after.checked_add(u64::try_from(removed_legacy_local_copies)?)
            == Some(doc_count_after_replay),
        "lexical cleanup changed unrelated live-document accounting; checkpoint retained"
    );

    // 5. Early/late endpoints against the published snapshot. Tokenless
    // messages must be verified too; unknown evidence cannot clear recovery.
    let early_canary_ok = published.verify(&early_doc)?;
    let late_canary_ok = published.verify(&late_doc)?;
    // Completed/resumed publication passes must pass this audit afresh. The
    // audit never opens a writer, refreshes the pinned view, or clears recovery.
    let verified_docs = projection.verify_published(&published, &checkpoint)?;

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
        removed_legacy_local_copies,
        verified_docs,
        verification_kind: "quill-idmap-schema-v1-xxh3-64-and-stored-columns",
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

    fn bound_projection(
        projection: &CanonicalProjection<'_>,
    ) -> Result<(LexicalReconcileCheckpoint, Vec<CassDocument>)> {
        let mut docs = Vec::new();
        let summary = projection.visit(|batch| {
            docs.extend_from_slice(batch);
            Ok(())
        })?;
        Ok((
            LexicalReconcileCheckpoint {
                version: checkpoint::VERSION,
                conversation_id: projection.conversation_id,
                source_id: projection.row.source_id.clone(),
                source_path: projection.row.source_path.to_string_lossy().into_owned(),
                message_count: summary.message_count,
                max_message_idx: summary.max_message_idx,
                content_bytes: summary.content_bytes,
                expected_docs: summary.expected_docs,
                projection_blake3: Some(checkpoint::projection_fingerprint(&docs)),
                started_at_ms: 1,
                attempt: 1,
            },
            docs,
        ))
    }

    #[test]
    #[serial_test::serial]
    fn full_audit_refuses_interior_loss_and_stale_text_despite_valid_endpoints() -> Result<()> {
        let tmp = TempDir::new()?;
        let prefix = "meridian evidence unicode λ ".repeat(1024);
        let (storage, row, provenance) = stored_projection(
            &tmp,
            vec![
                message(0, "meridian early endpoint"),
                message(7, &format!("{prefix}alpha")),
                message(13, "meridian late endpoint"),
            ],
        )?;
        let projection = CanonicalProjection {
            storage: &storage,
            row: &row,
            provenance: &provenance,
            conversation_id: row.id.context("test identity")?,
            max_content_bytes: 64,
            max_messages: 2,
        };
        let (checkpoint, docs) = bound_projection(&projection)?;
        assert_eq!(docs.len(), 3);
        for case in 0..2 {
            let path = tmp.path().join(format!("index-{case}"));
            let mut index = TantivyIndex::open_or_create(&path)?;
            let mut actual = docs.clone();
            if case == 0 {
                // A foreign identity balances the count of the missing middle.
                actual[1].conversation_id = Some(projection.conversation_id + 100);
            } else {
                actual[1].content = format!("{prefix}bravo");
                assert_eq!(actual[1].content.len(), docs[1].content.len());
            }
            index.add_prebuilt_documents_slice(&actual)?;
            index.commit()?;
            let checkpoint_path =
                lexical_reconcile_checkpoint_path(&path, projection.conversation_id);
            super::super::write_json_pretty_atomically(&checkpoint_path, &checkpoint)?;
            let recovery = std::fs::read(&checkpoint_path)?;
            let manifest = std::fs::read(path.join("MANIFEST"))?;
            let published = canary::exact::PublishedSnapshot::open(&path)?;
            assert_eq!(published.doc_count(), 3);
            assert!(published.verify(&docs[0])?);
            assert!(published.verify(&docs[2])?);
            if case == 1 {
                assert!(
                    published.verify(&docs[1])?,
                    "the preview comparator must pass"
                );
            }
            let error = projection
                .verify_published(&published, &checkpoint)
                .unwrap_err();
            assert!(error.to_string().contains("message 7"));
            assert!(error.to_string().contains("checkpoint retained"));
            assert_eq!(std::fs::read(&checkpoint_path)?, recovery);
            assert_eq!(std::fs::read(path.join("MANIFEST"))?, manifest);
            // Repairing the actual omitted/changed row admits the audit again;
            // unrelated identities do not count as witnesses for this source.
            index.upsert_prebuilt_documents_slice(&docs)?;
            index.commit()?;
            let repaired = canary::exact::PublishedSnapshot::open(&path)?;
            assert_eq!(projection.verify_published(&repaired, &checkpoint)?, 3);
        }
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn full_audit_is_snapshot_pinned_and_batch_independent() -> Result<()> {
        let tmp = TempDir::new()?;
        let (storage, row, provenance) = stored_projection(
            &tmp,
            vec![
                message(0, "meridian early endpoint"),
                message(3, ""),
                message(7, "meridian middle λ"),
                message(13, &"meridian late oversized ".repeat(300)),
            ],
        )?;
        let projection = CanonicalProjection {
            storage: &storage,
            row: &row,
            provenance: &provenance,
            conversation_id: row.id.context("test identity")?,
            max_content_bytes: 64,
            max_messages: 2,
        };
        let (checkpoint, docs) = bound_projection(&projection)?;
        assert_eq!(docs.len(), 3);
        let path = tmp.path().join("index");
        let mut index = TantivyIndex::open_or_create(&path)?;
        index.add_prebuilt_documents_slice(&docs)?;
        index.commit()?;
        let before = canary::exact::PublishedSnapshot::open(&path)?;
        let original_manifest = std::fs::read(path.join("MANIFEST"))?;
        for (max_messages, max_content_bytes) in [(1, 32), (2, 64), (8, 32_768)] {
            let repartitioned = CanonicalProjection {
                max_messages,
                max_content_bytes,
                ..projection
            };
            assert_eq!(
                repartitioned.verify_published(&before, &checkpoint)?,
                docs.len()
            );
        }
        assert_eq!(std::fs::read(path.join("MANIFEST"))?, original_manifest);
        let mut changed = docs[1].clone();
        changed.content = "meridian different middle".into();
        index.upsert_prebuilt_documents_slice(&[changed])?;
        index.commit()?;
        let after = canary::exact::PublishedSnapshot::open(&path)?;
        assert_eq!(
            projection.verify_published(&before, &checkpoint)?,
            docs.len()
        );
        assert!(projection.verify_published(&after, &checkpoint).is_err());
        for case in 0..4 {
            let mut wrong = checkpoint.clone();
            match case {
                0 => wrong.projection_blake3 = Some("0".repeat(64)),
                1 => wrong.expected_docs += 1,
                2 => wrong.content_bytes += 1,
                _ => wrong.conversation_id += 1,
            }
            assert!(
                projection.verify_published(&before, &wrong).is_err(),
                "binding {case}"
            );
        }
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn full_audit_production_reconcile_reports_verified_documents_before_clearing() -> Result<()> {
        let tmp = TempDir::new()?;
        let (storage, row, _) = stored_projection(
            &tmp,
            vec![
                message(0, "meridian early endpoint"),
                message(7, "meridian middle λ"),
                message(13, "1234 5678"),
            ],
        )?;
        let conversation_id = row.id.context("test identity")?;
        drop(storage);
        let data = tmp.path().join("data");
        std::fs::create_dir(&data)?;
        let report = run_lexical_conversation_reconcile(
            &data,
            &tmp.path().join("archive.db"),
            conversation_id,
        )?;
        assert_eq!(report.expected_docs, 3);
        assert_eq!(report.verified_docs, report.expected_docs);
        assert!(report.converged);
        assert_eq!(report.early_canary_ok, Some(true));
        assert_eq!(report.late_canary_ok, Some(true));
        assert!(report.checkpoint_cleared);
        let checkpoint_path =
            lexical_reconcile_checkpoint_path(&expected_index_dir(&data), conversation_id);
        assert!(!checkpoint_path.exists());
        assert!(!checkpoint_path.with_extension("progress.json").exists());
        let json = serde_json::to_value(report)?;
        assert_eq!(json["verified_docs"], 3);
        assert_eq!(json["removed_legacy_local_copies"], 0);
        assert_eq!(
            json["verification_kind"],
            "quill-idmap-schema-v1-xxh3-64-and-stored-columns"
        );
        Ok(())
    }

    #[test]
    fn canonical_packet_provenance_overrides_stale_metadata() {
        for (source_id, origin_kind, origin_host) in [
            ("laptop", "remote", Some("laptop.example")),
            ("full-history-plan", "local", None),
            ("local", "local", None),
        ] {
            let mut canonical = conversation(vec![message(0, "canonical provenance proof")]);
            canonical.source_id = source_id.into();
            canonical.origin_host = origin_host.map(str::to_owned);
            // Historical connector metadata is neither required nor allowed
            // to override the canonical columns and registered source kind.
            for metadata in [
                serde_json::Value::Null,
                serde_json::json!({"cass":{"origin":{
                    "source_id":"stale-source", "kind":"ssh", "host":"stale-host"
                }}}),
            ] {
                canonical.metadata_json = metadata;
                let packet = ConversationPacket::from_canonical_replay(
                    &canonical,
                    ConversationPacketProvenance {
                        source_id: source_id.into(),
                        origin_kind: origin_kind.into(),
                        origin_host: origin_host.map(str::to_owned),
                    },
                );
                let docs = TantivyIndex::build_packet_documents(&packet, canonical.id);
                assert_eq!(docs.len(), 1);
                assert_eq!(docs[0].source_id, source_id);
                assert_eq!(docs[0].origin_kind, origin_kind);
                assert_eq!(docs[0].origin_host.as_deref(), origin_host);
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn public_reconcile_preserves_canonical_source_identity() -> Result<()> {
        assert_public_reconcile_source_identity(false)
    }

    #[test]
    #[serial_test::serial]
    fn public_reconcile_removes_witnessed_legacy_local_copies() -> Result<()> {
        assert_public_reconcile_source_identity(true)
    }

    fn assert_public_reconcile_source_identity(seed_legacy_local_copies: bool) -> Result<()> {
        use crate::search::query::{FieldMask, SearchClient, SearchFilters};
        use crate::sources::provenance::{Source, SourceFilter};

        let temp = TempDir::new()?;
        let db_path = temp.path().join("archive.db");
        let repaired_data = temp.path().join("repaired");
        let rebuilt_data = temp.path().join("rebuilt");
        std::fs::create_dir_all(&repaired_data)?;
        std::fs::create_dir_all(&rebuilt_data)?;
        let storage = FrankenStorage::open(&db_path)?;
        let agent = storage.ensure_agent(&Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: None,
            kind: AgentKind::Cli,
        })?;
        let mut named_local = Source::local();
        named_local.id = "full-history-plan".into();
        let sources = [
            Source::remote("laptop", "laptop.example"),
            named_local,
            Source::local(),
        ];
        let mut conversations = Vec::new();
        let mut retained = Vec::new();
        for source in &sources {
            storage.upsert_source(source)?;
            let mut canonical = conversation(vec![
                message(0, "provenanceproof missing prefix"),
                message(7, "provenanceproof retained tail"),
            ]);
            canonical.id = None;
            canonical.external_id = Some(format!("source-proof-{}", source.id));
            // An independent local history shares the remote transcript path
            // and message text; its different canonical conversation ID must
            // keep it out of the narrowly witnessed cleanup.
            canonical.source_path = PathBuf::from(format!(
                "/archive/{}.jsonl",
                if source.id == LOCAL_SOURCE_ID {
                    "laptop"
                } else {
                    &source.id
                }
            ));
            canonical.source_id = source.id.clone();
            canonical.origin_host = source.host_label.clone();
            canonical.metadata_json = serde_json::json!({"cass":{"origin":{
                "source_id":"obsolete-source", "kind":"ssh", "host":"obsolete-host"
            }}});
            let id = storage
                .insert_conversation_tree(agent, None, &canonical)?
                .conversation_id;
            conversations.push(id);
            // Seed an independently constructed, correctly attributed tail.
            // Repair must add the missing prefix under this same identity,
            // without manufacturing a second local copy of the healthy tail.
            let tail = &canonical.messages[1];
            retained.push(CassDocument {
                agent: canonical.agent_slug.clone(),
                workspace: None,
                workspace_original: None,
                source_path: canonical.source_path.to_string_lossy().into_owned(),
                msg_idx: tail.idx as u64,
                created_at: tail.created_at,
                title: canonical.title.clone(),
                content: tail.content.clone(),
                source_id: source.id.clone(),
                origin_kind: if source.is_remote() {
                    "remote"
                } else {
                    "local"
                }
                .into(),
                origin_host: source.host_label.clone(),
                conversation_id: Some(id),
            });
            if seed_legacy_local_copies && source.id != LOCAL_SOURCE_ID {
                let template = retained.last().context("retained tail")?.clone();
                for message in &canonical.messages {
                    retained.push(CassDocument {
                        source_id: LOCAL_SOURCE_ID.into(),
                        origin_kind: "local".into(),
                        origin_host: None,
                        msg_idx: message.idx as u64,
                        created_at: message.created_at,
                        content: message.content.clone(),
                        ..template.clone()
                    });
                }
            }
        }
        storage.close()?;
        super::super::rebuild_tantivy_from_db(&db_path, &rebuilt_data, 2, None)?;
        let repaired_path = expected_index_dir(&repaired_data);
        let mut index = TantivyIndex::open_or_create(&repaired_path)?;
        index.add_prebuilt_documents_slice(&retained)?;
        index.commit()?;
        assert_eq!(
            index.doc_count()?,
            if seed_legacy_local_copies { 7 } else { 3 }
        );
        drop(index);

        let search = |index_path: &Path, source_filter: SourceFilter| -> Result<Vec<String>> {
            // No canonical fallback: these results must come from the actual
            // published lexical identities and stored source metadata.
            let client = SearchClient::open(index_path, None)?.context("published test index")?;
            let mut hits = client
                .search(
                    "provenanceproof",
                    SearchFilters {
                        source_filter,
                        ..SearchFilters::default()
                    },
                    20,
                    0,
                    FieldMask::FULL,
                )?
                .into_iter()
                .map(|hit| {
                    serde_json::json!({
                        "conversation_id": hit.conversation_id,
                        "message_index": hit.line_number,
                        "source_id": hit.source_id,
                        "origin_kind": hit.origin_kind,
                        "origin_host": hit.origin_host,
                        "source_path": hit.source_path,
                        "content": hit.content,
                    })
                    .to_string()
                })
                .collect::<Vec<_>>();
            hits.sort();
            Ok(hits)
        };
        for attempt in 0..2 {
            for (source, &id) in sources.iter().zip(&conversations) {
                let report = run_lexical_conversation_reconcile(&repaired_data, &db_path, id)?;
                assert!(report.checkpoint_cleared);
                assert_eq!(report.verified_docs, 2);
                let expected_removals =
                    if seed_legacy_local_copies && attempt == 0 && source.id != LOCAL_SOURCE_ID {
                        2
                    } else {
                        0
                    };
                assert_eq!(report.removed_legacy_local_copies, expected_removals);
            }
            for (filter, count) in [
                (SourceFilter::All, 6),
                (SourceFilter::Remote, 2),
                (SourceFilter::Local, 4),
                (SourceFilter::SourceId("laptop".into()), 2),
                (SourceFilter::SourceId("full-history-plan".into()), 2),
                (SourceFilter::SourceId(LOCAL_SOURCE_ID.into()), 2),
                (SourceFilter::SourceId("obsolete-source".into()), 0),
            ] {
                let expected = search(&expected_index_dir(&rebuilt_data), filter.clone())?;
                assert_eq!(expected.len(), count, "full rebuild control: {filter:?}");
                assert_eq!(
                    search(&repaired_path, filter.clone())?,
                    expected,
                    "{filter:?}"
                );
            }
        }
        let published = canary::exact::PublishedSnapshot::open(&repaired_path)?;
        assert_eq!(
            published.doc_count(),
            6,
            "repair must not duplicate source identities"
        );
        Ok(())
    }

    fn cleanup_document(message_index: u64) -> CassDocument {
        CassDocument {
            agent: "codex".into(),
            workspace: None,
            workspace_original: None,
            source_path: "/archive/cleanup.jsonl".into(),
            msg_idx: message_index,
            created_at: Some(1_700_000_000_000),
            title: Some("legacy cleanup".into()),
            content: format!("legacyguard canonical message {message_index}"),
            source_id: "laptop".into(),
            origin_kind: "remote".into(),
            origin_host: Some("laptop.example".into()),
            conversation_id: Some(42),
        }
    }

    fn legacy_local_copy(document: &CassDocument) -> CassDocument {
        CassDocument {
            source_id: LOCAL_SOURCE_ID.into(),
            origin_kind: "local".into(),
            origin_host: None,
            ..document.clone()
        }
    }

    #[test]
    #[serial_test::serial]
    fn legacy_local_cleanup_preserves_different_content_and_identity_collisions() -> Result<()> {
        use crate::search::query::{FieldMask, SearchClient, SearchFilters};
        use crate::sources::provenance::SourceFilter;

        let temp = TempDir::new()?;
        let index_path = expected_index_dir(temp.path());
        let canonical = (0..3).map(cleanup_document).collect::<Vec<_>>();
        let matching = legacy_local_copy(&canonical[0]);
        let mut different_content = legacy_local_copy(&canonical[1]);
        different_content.content = "legacyguard unrelated local history".into();
        let mut collision = legacy_local_copy(&canonical[2]);
        let expected_collision = cass_document_identity(
            &collision.source_id,
            CassConversationKey::for_document(collision.as_ref()),
            collision.msg_idx,
        );
        // Retained v9 identities concatenate separators. This independent
        // path without a conversation ID has the same opaque key as the
        // proposed legacy copy, but different native stored metadata.
        collision.source_path.push_str("#42");
        collision.conversation_id = None;
        assert_eq!(
            cass_document_identity(
                &collision.source_id,
                CassConversationKey::for_document(collision.as_ref()),
                collision.msg_idx,
            ),
            expected_collision
        );
        let mut index = TantivyIndex::open_or_create(&index_path)?;
        index.add_prebuilt_documents_slice(&canonical)?;
        index.add_prebuilt_documents_slice(&[
            matching.clone(),
            different_content.clone(),
            collision.clone(),
        ])?;
        index.commit()?;
        assert_eq!(remove_legacy_local_copy_batch(&mut index, &canonical)?, 1);
        assert_eq!(remove_legacy_local_copy_batch(&mut index, &canonical)?, 0);
        assert_eq!(index.doc_count()?, 5);
        drop(index);

        let published = canary::exact::PublishedSnapshot::open(&index_path)?;
        assert!(!published.verify_content(&matching)?);
        assert!(published.verify_content(&different_content)?);
        assert!(published.verify_content(&collision)?);
        for document in &canonical {
            assert!(published.verify_content(document)?);
        }
        let client = SearchClient::open(&index_path, None)?.context("cleanup test index")?;
        let hits = client.search(
            "legacyguard",
            SearchFilters {
                source_filter: SourceFilter::Local,
                ..SearchFilters::default()
            },
            20,
            0,
            FieldMask::FULL,
        )?;
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().any(|hit| {
            hit.source_path == different_content.source_path
                && hit.content == different_content.content
        }));
        assert!(hits.iter().any(|hit| {
            hit.source_path == collision.source_path
                && hit.conversation_id == collision.conversation_id
                && hit.content == collision.content
        }));
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn legacy_local_cleanup_requires_full_authoritative_counterpart() -> Result<()> {
        let temp = TempDir::new()?;
        let index_path = expected_index_dir(temp.path());
        let canonical = cleanup_document(0);
        let legacy = legacy_local_copy(&canonical);
        let mut index = TantivyIndex::open_or_create(&index_path)?;
        index.add_prebuilt_documents_slice(std::slice::from_ref(&legacy))?;
        index.commit()?;
        for stale_counterpart in [false, true] {
            if stale_counterpart {
                let mut stale = canonical.clone();
                stale.content = "legacyguard stale authoritative payload".into();
                index.add_prebuilt_documents_slice(&[stale])?;
                index.commit()?;
            }
            let count = index.doc_count()?;
            let manifest = std::fs::read(index_path.join("MANIFEST"))?;
            let error =
                remove_legacy_local_copy_batch(&mut index, std::slice::from_ref(&canonical))
                    .expect_err("missing or stale authority cannot authorize deletion");
            assert!(
                error
                    .to_string()
                    .contains("counterpart is missing or stale")
            );
            assert_eq!(index.doc_count()?, count);
            assert_eq!(std::fs::read(index_path.join("MANIFEST"))?, manifest);
            let published =
                canary::exact::PublishedSnapshot::from_keeper(index.committed_snapshot()?)?;
            assert!(published.verify_content(&legacy)?);
        }
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn legacy_local_cleanup_refuses_pending_index_mutations() -> Result<()> {
        let temp = TempDir::new()?;
        let index_path = expected_index_dir(temp.path());
        let canonical = cleanup_document(0);
        let legacy = legacy_local_copy(&canonical);
        let unrelated = cleanup_document(7);
        let mut index = TantivyIndex::open_or_create(&index_path)?;
        index.add_prebuilt_documents_slice(&[canonical.clone(), legacy])?;
        index.commit()?;
        let manifest = std::fs::read(index_path.join("MANIFEST"))?;
        index.add_prebuilt_documents_slice(std::slice::from_ref(&unrelated))?;
        let error = remove_legacy_local_copy_batch(&mut index, std::slice::from_ref(&canonical))
            .expect_err("cleanup cannot commit pending changes after witnessing a generation");
        assert!(error.to_string().contains("fully committed generation"));
        assert_eq!(index.doc_count()?, 2);
        assert_eq!(std::fs::read(index_path.join("MANIFEST"))?, manifest);

        index.commit()?;
        assert_eq!(index.doc_count()?, 3);
        assert_eq!(
            remove_legacy_local_copy_batch(&mut index, std::slice::from_ref(&canonical))?,
            1
        );
        let published = canary::exact::PublishedSnapshot::from_keeper(index.committed_snapshot()?)?;
        assert!(published.verify_content(&canonical)?);
        assert!(published.verify_content(&unrelated)?);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn legacy_local_cleanup_refuses_multiple_live_identity_rows() -> Result<()> {
        use frankensearch::quill::KeeperError;

        let temp = TempDir::new()?;
        let index_path = expected_index_dir(temp.path());
        let canonical = cleanup_document(0);
        let legacy = legacy_local_copy(&canonical);
        let identity = cass_document_identity(
            &legacy.source_id,
            CassConversationKey::for_document(legacy.as_ref()),
            legacy.msg_idx,
        );
        let mut index = TantivyIndex::open_or_create(&index_path)?;
        index.add_prebuilt_documents_slice(&[canonical.clone(), legacy.clone()])?;
        index.commit()?;
        let old_id = index
            .committed_snapshot()?
            .resolve_document_id(&identity)?
            .context("original legacy identity")?
            .global_docid;
        let mut different = legacy;
        different.content = "legacyguard independent same-ID payload".into();
        index.upsert_prebuilt_documents_slice(&[different])?;
        index.commit()?;
        let mut manifest = index
            .committed_snapshot()?
            .loaded_manifest()
            .manifest
            .clone();
        drop(index);

        // Reproduce a corrupt historical publication with two live physical
        // rows behind one opaque ID: one matches the candidate and the other
        // does not. Native resolution must reject the ambiguity instead of
        // selecting a matching row and authorizing deletion of its sibling.
        let segment = manifest
            .segments
            .iter_mut()
            .find(|segment| segment.tombstones.contains(old_id))
            .context("upsert tombstoned the original physical row")?;
        assert!(segment.tombstones.remove(old_id)?);
        let bytes = manifest.to_bytes()?;
        std::fs::write(index_path.join("MANIFEST"), &bytes)?;
        let mut index = TantivyIndex::open_or_create(&index_path)?;
        assert_eq!(index.doc_count()?, 3);
        let error = remove_legacy_local_copy_batch(&mut index, std::slice::from_ref(&canonical))
            .expect_err("ambiguous native identities must refuse cleanup");
        assert!(matches!(
            error.downcast_ref::<KeeperError>(),
            Some(KeeperError::MultipleLiveDocumentIds { .. })
        ));
        assert_eq!(index.doc_count()?, 3);
        assert_eq!(std::fs::read(index_path.join("MANIFEST"))?, bytes);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn legacy_local_cleanup_bounds_identity_storage_before_mutation() -> Result<()> {
        let temp = TempDir::new()?;
        let index_path = expected_index_dir(temp.path());
        let canonical = cleanup_document(0);
        let legacy = legacy_local_copy(&canonical);
        let mut index = TantivyIndex::open_or_create(&index_path)?;
        index.add_prebuilt_documents_slice(&[canonical.clone(), legacy])?;
        index.commit()?;
        let manifest = std::fs::read(index_path.join("MANIFEST"))?;
        let mut oversized = canonical.clone();
        oversized.source_path = "x".repeat(LEGACY_CLEANUP_MAX_ID_BYTES);
        for batch in [
            vec![oversized],
            vec![canonical; LEGACY_CLEANUP_MAX_IDENTITIES + 1],
        ] {
            let error = remove_legacy_local_copy_batch(&mut index, &batch)
                .expect_err("oversized cleanup admission must precede any mutation");
            assert!(error.to_string().contains("identity budget"));
            assert_eq!(index.doc_count()?, 2);
            assert_eq!(std::fs::read(index_path.join("MANIFEST"))?, manifest);
        }
        Ok(())
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
                bail!("consumer failed");
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
