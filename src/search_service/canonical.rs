//! Explicit canonical follow-up for a fixed, operator-selected archive.
//!
//! Metadata selection and bounded body reads share one read transaction. This
//! is a new archive snapshot, NOT a certificate for the retained lexical index.
//! Source paths are compared as identity strings and are never opened as files.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use coding_agent_search::franken_sync::compat::{ConnectionExt, RowExt};
use coding_agent_search::franken_sync::params;
use coding_agent_search::storage::sqlite::FrankenStorage;
use serde_json::{Value, json};

use super::protocol::MAX_IDENTITY_BYTES;

pub(super) const MAX_CONTEXT: usize = 20;
pub(super) const MAX_CONTENT_BYTES: usize = 64 * 1024;
const MAX_ROLE_BYTES: usize = 128;
const LOOKUP_BUDGET: Duration = Duration::from_secs(3);

pub(super) struct View<'a> {
    pub source_path: &'a str,
    pub source_id: &'a str,
    pub conversation_id: i64,
    pub message_index: u64,
    pub context: usize,
}

impl View<'_> {
    pub(super) fn validate(&self) -> Result<(), &'static str> {
        if self.source_path.trim().is_empty() || self.source_path.len() > MAX_IDENTITY_BYTES {
            return Err("source_path must be nonempty and at most 4096 UTF-8 bytes");
        }
        if self.source_id.is_empty()
            || self.source_id.trim() != self.source_id
            || self.source_id.len() > MAX_IDENTITY_BYTES
        {
            return Err("source_id must be an unpadded, nonempty exact ID of at most 4096 bytes");
        }
        if self.conversation_id <= 0 {
            return Err("conversation_id must be a positive canonical database ID");
        }
        if self.message_index.checked_sub(1).and_then(|idx| i64::try_from(idx).ok()).is_none() {
            return Err("message_index must be a one-based canonical index representable by the archive");
        }
        if self.context > MAX_CONTEXT {
            return Err("context must be between 0 and 20 actual messages on each side");
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
enum Refusal {
    #[error("the requested canonical conversation or message no longer exists; no neighbour was substituted")]
    NotFound,
    #[error("source_path or source_id does not exactly match the requested canonical conversation")]
    IdentityMismatch,
    #[error("canonical message coordinates are ambiguous or invalid; no message was selected")]
    InvalidCoordinates,
    #[error("the complete canonical window exceeds 64 KiB of message content; narrow context or use the explicit CLI follow-up")]
    PayloadTooLarge,
    #[error("canonical lookup exceeded its cooperative 3-second budget; no partial window was returned")]
    Deadline,
}

pub(super) fn error_kind(error: &anyhow::Error) -> &'static str {
    match error.downcast_ref::<Refusal>() {
        Some(Refusal::NotFound) => "canonical_not_found",
        Some(Refusal::IdentityMismatch) => "canonical_identity_mismatch",
        Some(Refusal::InvalidCoordinates) => "canonical_invalid_coordinates",
        Some(Refusal::PayloadTooLarge) => "canonical_payload_too_large",
        Some(Refusal::Deadline) => "canonical_deadline",
        None => "canonical_lookup_failed",
    }
}

fn check_budget(started: Instant) -> Result<()> {
    if started.elapsed() >= LOOKUP_BUDGET {
        return Err(Refusal::Deadline.into());
    }
    Ok(())
}

struct Snapshot<'a> {
    storage: &'a FrankenStorage,
    active: bool,
}

impl<'a> Snapshot<'a> {
    fn begin(storage: &'a FrankenStorage) -> Result<Self> {
        storage.raw().execute("BEGIN DEFERRED").context("begin canonical read snapshot")?;
        Ok(Self { storage, active: true })
    }

    fn release(mut self) -> Result<()> {
        self.storage.raw().execute("ROLLBACK").context("release canonical read snapshot")?;
        self.active = false;
        Ok(())
    }
}

impl Drop for Snapshot<'_> {
    fn drop(&mut self) {
        if self.active && let Err(error) = self.storage.raw().execute("ROLLBACK") {
            tracing::warn!(%error, "failed to release canonical service read snapshot");
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Anchor {
    id: i64,
    idx: i64,
}

/// Read only through the established strict storage opener. Admission and one
/// native SQL call are not preemptible here; deadlines are checked between
/// calls. The host continues to own the process-level deadline and memory cap.
pub(super) fn read(db: &Path, request: &View<'_>) -> Result<Value> {
    request.validate().map_err(anyhow::Error::msg)?;
    let started = Instant::now();
    let metadata = std::fs::symlink_metadata(db).context("inspect configured canonical archive")?;
    ensure!(metadata.file_type().is_file(), "configured canonical archive must be a regular file, not a symlink");
    let storage = FrankenStorage::open_strict_readonly(db).context("open configured archive strictly read-only")?;
    check_budget(started)?;
    let snapshot = Snapshot::begin(&storage)?;
    let result = read_snapshot(&storage, request, started);
    let released = snapshot.release();
    // Preserve the primary typed failure, including its original storage cause.
    match result {
        Err(error) => Err(error),
        Ok(value) => {
            released?;
            Ok(value)
        }
    }
}

fn read_snapshot(storage: &FrankenStorage, request: &View<'_>, started: Instant) -> Result<Value> {
    check_budget(started)?;
    // A fixed primary-key lookup cannot resolve an identically named session
    // in another source. Compare literal path/source BEFORE any body read.
    let identities = storage.raw().query_map_collect(
        "SELECT source_path, source_id FROM conversations WHERE id = ?1 LIMIT 2",
        params![request.conversation_id],
        |row| Ok((row.get_typed::<String>(0)?, row.get_typed::<String>(1)?)),
    ).context("read canonical conversation identity")?;
    let Some((path, source)) = identities.first() else { return Err(Refusal::NotFound.into()); };
    if identities.len() != 1 { return Err(Refusal::InvalidCoordinates.into()); }
    if path != request.source_path || source != request.source_id {
        return Err(Refusal::IdentityMismatch.into());
    }
    let idx = i64::try_from(request.message_index - 1)?;
    check_budget(started)?;
    let targets = storage.raw().query_map_collect(
        "SELECT id, idx FROM messages WHERE conversation_id = ?1 AND idx = ?2 LIMIT 2",
        params![request.conversation_id, idx],
        |row| Ok(Anchor { id: row.get_typed(0)?, idx: row.get_typed(1)? }),
    ).context("select exact canonical message")?;
    let Some(target) = targets.first().copied() else { return Err(Refusal::NotFound.into()); };
    if targets.len() != 1 || target.id <= 0 || target.idx != idx {
        return Err(Refusal::InvalidCoordinates.into());
    }
    let mut anchors = Vec::new();
    let mut more_before = None;
    let mut more_after = None;
    if request.context > 0 {
        // LIMIT bounds metadata retained across the engine boundary. Sparse
        // canonical indices count as actual messages, not idx +/- context.
        let count = i64::try_from(request.context + 1)?;
        check_budget(started)?;
        let mut before = storage.raw().query_map_collect(
            "SELECT id, idx FROM messages WHERE conversation_id = ?1 AND idx < ?2 ORDER BY idx DESC LIMIT ?3",
            params![request.conversation_id, idx, count],
            |row| Ok(Anchor { id: row.get_typed(0)?, idx: row.get_typed(1)? }),
        ).context("select preceding canonical message IDs")?;
        if before.len() > request.context + 1
            || before.iter().any(|anchor| anchor.id <= 0 || anchor.idx < 0 || anchor.idx >= idx)
            || before.windows(2).any(|pair| pair[0].idx <= pair[1].idx)
        {
            return Err(Refusal::InvalidCoordinates.into());
        }
        more_before = Some(before.len() > request.context);
        before.truncate(request.context);
        before.reverse();
        anchors.extend(before);
        anchors.push(target);
        check_budget(started)?;
        let mut after = storage.raw().query_map_collect(
            "SELECT id, idx FROM messages WHERE conversation_id = ?1 AND idx > ?2 ORDER BY idx LIMIT ?3",
            params![request.conversation_id, idx, count],
            |row| Ok(Anchor { id: row.get_typed(0)?, idx: row.get_typed(1)? }),
        ).context("select following canonical message IDs")?;
        if after.len() > request.context + 1
            || after.iter().any(|anchor| anchor.id <= 0 || anchor.idx <= idx)
            || after.windows(2).any(|pair| pair[0].idx >= pair[1].idx)
        {
            return Err(Refusal::InvalidCoordinates.into());
        }
        more_after = Some(after.len() > request.context);
        after.truncate(request.context);
        anchors.extend(after);
    } else {
        anchors.push(target);
    }
    ensure!(anchors.len() <= 2 * request.context + 1, "canonical metadata exceeded the requested window");
    if anchors.iter().any(|anchor| anchor.id <= 0 || anchor.idx < 0)
        || anchors.windows(2).any(|pair| pair[0].idx >= pair[1].idx)
    {
        return Err(Refusal::InvalidCoordinates.into());
    }
    let mut content_bytes = 0_usize;
    let mut messages = Vec::with_capacity(anchors.len());
    for anchor in anchors {
        check_budget(started)?;
        let remaining = i64::try_from(MAX_CONTENT_BYTES - content_bytes)?;
        // Check byte lengths inside SQL BEFORE transferring bodies to the host.
        // An oversized/invalid body is never shortened into successful evidence.
        // This is not a guarantee about the engine's internal page allocations.
        let rows = storage.raw().query_map_collect(
            "SELECT id, idx, typeof(content), length(CAST(content AS BLOB)),
             CASE WHEN typeof(content) = 'text' AND length(CAST(content AS BLOB)) <= ?4 THEN content ELSE NULL END,
             CASE WHEN typeof(role) = 'text' AND length(CAST(role AS BLOB)) <= ?5 THEN role ELSE NULL END
             FROM messages WHERE conversation_id = ?1 AND id = ?2 AND idx = ?3 LIMIT 2",
            params![request.conversation_id, anchor.id, anchor.idx, remaining, MAX_ROLE_BYTES as i64],
            |row| Ok((
                row.get_typed::<i64>(0)?, row.get_typed::<i64>(1)?,
                row.get_typed::<String>(2)?, row.get_typed::<Option<i64>>(3)?,
                row.get_typed::<Option<String>>(4)?, row.get_typed::<Option<String>>(5)?,
            )),
        ).context("hydrate bounded canonical message")?;
        ensure!(rows.len() == 1, "canonical message identity changed within the read snapshot");
        let (id, index, kind, length, content, role) = rows.into_iter().next().context("missing canonical body")?;
        ensure!(id == anchor.id && index == anchor.idx, "canonical hydration returned a different message");
        ensure!(kind == "text", "canonical message content must be text, not {kind}");
        let length = usize::try_from(length.context("canonical content has no byte length")?)?;
        if length > MAX_CONTENT_BYTES - content_bytes { return Err(Refusal::PayloadTooLarge.into()); }
        let content = content.context("canonical content was refused by the bounded projection")?;
        ensure!(content.len() == length, "canonical byte length differs from its complete content");
        let role = role.context("canonical role must be text of at most 128 bytes")?;
        let role = if role.eq_ignore_ascii_case("agent") { "assistant".to_string() } else { role };
        content_bytes += length;
        messages.push(json!({
            "message_id": id, "message_index": (index as u64) + 1,
            "role": role, "content": content, "is_target": id == target.id,
        }));
    }
    check_budget(started)?;
    Ok(json!({
        "source_path": request.source_path, "source_id": request.source_id,
        "conversation_id": request.conversation_id,
        "message_index": request.message_index, "context": request.context,
        "coordinate_space": "message_index", "content_source": "canonical_archive",
        "messages": messages, "content_bytes": content_bytes,
        "more_before": more_before, "more_after": more_after,
        "snapshot_policy": "one_archive_read_transaction_per_view",
        "matches_lexical_snapshot": null, "lexical_freshness": "not_checked",
        "preview_only": false, "maintenance_performed": false,
    }))
}

#[cfg(test)]
#[path = "canonical_tests.rs"]
mod tests;
