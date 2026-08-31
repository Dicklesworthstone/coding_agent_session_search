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
//!    source fingerprint (message count, max idx, capped content bytes);
//! 2. persist a durable recovery checkpoint with the expected doc count
//!    BEFORE any publication;
//! 3. upsert the full source doc set under Quill's stable CASS document
//!    identities (source id + source path + conversation id + msg idx),
//!    then publish a successor generation;
//! 4. on retry, re-read the checkpoint and converge to exactly one live doc
//!    per identity (upsert replaces; never appends);
//! 5. verify early/late canaries and the live-doc count before clearing the
//!    durable checkpoint.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::search::asset_state::SearchMaintenanceMode;
use crate::search::tantivy::{TantivyIndex, expected_index_dir};
use crate::storage::sqlite::FrankenStorage;

/// Durable recovery checkpoint written before the first publication and
/// cleared only after convergence + canary verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LexicalReconcileCheckpoint {
    pub version: u32,
    pub conversation_id: i64,
    pub source_id: String,
    pub source_path: String,
    /// Immutable source fingerprint: capped message rows the reconcile bound.
    pub message_count: usize,
    pub max_message_idx: i64,
    pub content_bytes: usize,
    /// Lexical docs the bound source set projects to (post noise filter).
    pub expected_docs: usize,
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
    /// True when a second upsert of the identical set left the live-doc
    /// count unchanged — the converge-to-one-doc-per-identity proof.
    pub converged: bool,
    /// Early/late content canaries observed in the published snapshot with a
    /// matching stored conversation id. `None` when the message carried no
    /// usable search token (vacuously accepted).
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
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if matches!(err.kind(), std::io::ErrorKind::NotFound) => return Ok(None),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("reading reconcile checkpoint {}", path.display()));
        }
    };
    serde_json::from_str(&raw)
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

/// Search the published snapshot for `token` and require a hit whose stored
/// conversation id matches. `Ok(None)` when no token was derivable.
fn verify_canary(
    index: &TantivyIndex,
    conversation_id: i64,
    token: Option<&str>,
) -> Result<Option<bool>> {
    let Some(token) = token else {
        return Ok(None);
    };
    let parser = frankensearch::quill::query::CassQueryParser::new(
        frankensearch::quill::schema::CASS_SEMANTIC_SCHEMA,
    )
    .map_err(|error| anyhow!("building the CASS query parser for canary: {error}"))?;
    let parsed = parser.parse(
        token,
        &frankensearch::quill::query::CassQueryFilters::default(),
    );
    let reader = index.reader()?;
    let page = crate::search::quill_bridge::search_paginated(&reader, &parsed.query, 25, 0, false)?;
    let fields = &index.fields;
    for hit in &page.hits {
        let stored = crate::search::quill_bridge::stored_i64(
            &reader,
            fields.conversation_id,
            hit.global_docid,
        )
        .unwrap_or(None);
        if stored.is_some_and(|id| id.cmp(&conversation_id).is_eq()) {
            return Ok(Some(true));
        }
    }
    Ok(Some(false))
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
            .unwrap_or_default()
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
    // the bound source set is byte-for-byte the same shape.
    let checkpoint_path = lexical_reconcile_checkpoint_path(&index_path, conversation_id);
    std::fs::create_dir_all(&index_path)
        .with_context(|| format!("creating index directory {}", index_path.display()))?;
    let attempt = match load_checkpoint(&checkpoint_path)? {
        Some(existing) => {
            let identity_matches = existing.source_id.cmp(&row.source_id).is_eq()
                && existing
                    .source_path
                    .cmp(&row.source_path.to_string_lossy().to_string())
                    .is_eq();
            let fingerprint_matches = matches!(
                (existing.message_count, existing.max_message_idx, existing.content_bytes),
                (mc, mi, cb) if mc.cmp(&message_count).is_eq()
                    && mi.cmp(&max_message_idx).is_eq()
                    && cb.cmp(&content_bytes).is_eq()
            );
            if !identity_matches || !fingerprint_matches {
                bail!(
                    "reconcile checkpoint {} was bound to a different source shape \
                     (checkpoint: {} msgs / max idx {} / {} bytes; live: {} / {} / {}); \
                     the canonical source changed — run a normal `cass index` instead, \
                     then retry, or remove the checkpoint to rebind",
                    checkpoint_path.display(),
                    existing.message_count,
                    existing.max_message_idx,
                    existing.content_bytes,
                    message_count,
                    max_message_idx,
                    content_bytes,
                );
            }
            existing.attempt.saturating_add(1)
        }
        None => 1,
    };
    let checkpoint = LexicalReconcileCheckpoint {
        version: 1,
        conversation_id,
        source_id: row.source_id.clone(),
        source_path: row.source_path.to_string_lossy().to_string(),
        message_count,
        max_message_idx,
        content_bytes,
        expected_docs: docs.len(),
        started_at_ms: FrankenStorage::now_millis(),
        attempt,
    };
    super::write_json_pretty_atomically(&checkpoint_path, &checkpoint)?;

    // 3. Upsert the full source doc set and publish a successor generation.
    let mut index = TantivyIndex::open_or_create(&index_path)?;
    let doc_count_before = index.doc_count()?;
    let upserted_docs = index.upsert_prebuilt_documents_slice(&docs)?;
    index.commit()?;
    let doc_count_after_first = index.doc_count()?;

    // 4. Converge proof: replaying the identical set must not grow the live
    // set — one live document per identity, never append.
    index.upsert_prebuilt_documents_slice(&docs)?;
    index.commit()?;
    let doc_count_after = index.doc_count()?;
    let converged = doc_count_after.cmp(&doc_count_after_first).is_eq();

    // 5. Early/late canaries against the published snapshot.
    let early_canary_ok = verify_canary(&index, conversation_id, early_token.as_deref())?;
    let late_canary_ok = verify_canary(&index, conversation_id, late_token.as_deref())?;

    let canaries_ok = early_canary_ok.unwrap_or(true) && late_canary_ok.unwrap_or(true);
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
        early_canary_ok,
        late_canary_ok,
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
        assert_eq!(verify_canary(&index, 42, early.as_deref())?, Some(true));
        assert_eq!(verify_canary(&index, 42, late.as_deref())?, Some(true));
        // A wrong conversation id must not satisfy the canary.
        assert_eq!(verify_canary(&index, 43, early.as_deref())?, Some(false));
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
