//! Bounded, forward-only canonical transcript pages, including large messages.
//!
//! Each page has its own read-only transaction. A cursor is a position with
//! identity/length checks, NOT a stable snapshot or a content certificate across
//! requests. In particular, same-length edits between pages are not detected.
//! Use a verified logical archive when a coherent whole-session export is needed.
//! SQL bounds bytes transferred to CASS, not the engine's internal allocations.

use super::{
    Anchor, Connection, ConnectionExt, Context, Path, Refusal as CanonicalRefusal, Result, RowExt,
    Value, View, check_budget, ensure, json, params, require_conversation_identity, with_snapshot,
};
use serde::{Deserialize, Serialize};
use std::time::Instant;

pub(in crate::search_service) const MAX_MESSAGES: usize = 32;
// Leave ample room for both MCP representations, including worst-case escaping
// of content, roles, and the two 4096-byte identity strings.
pub(in crate::search_service) const MAX_PAGE_BYTES: usize = 32 * 1024;

pub(in crate::search_service) fn default_limit() -> usize {
    16
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::search_service) struct Cursor {
    pub message_id: i64,
    pub message_index: u64,
    pub byte_offset: u64,
    /// Present for a partially emitted message; absent before the next body is read.
    pub content_bytes: Option<u64>,
}

impl Cursor {
    fn at(anchor: Anchor) -> Self {
        Self {
            message_id: anchor.id,
            message_index: (anchor.idx as u64) + 1,
            byte_offset: 0,
            content_bytes: None,
        }
    }

    fn validate(&self) -> Result<(), &'static str> {
        if self.message_id <= 0
            || self
                .message_index
                .checked_sub(1)
                .and_then(|n| i64::try_from(n).ok())
                .is_none()
            || self.byte_offset >= i64::MAX as u64
            || self.content_bytes.is_some_and(|n| n > i64::MAX as u64)
        {
            return Err("transcript cursor coordinates are outside the canonical range");
        }
        if self.byte_offset != 0
            && self
                .content_bytes
                .is_none_or(|bytes| self.byte_offset >= bytes)
        {
            return Err(
                "a partial-message cursor requires its original content_bytes above byte_offset",
            );
        }
        Ok(())
    }
}

pub(in crate::search_service) struct Page<'a> {
    pub source_path: &'a str,
    pub source_id: &'a str,
    pub conversation_id: i64,
    pub cursor: Option<&'a Cursor>,
    pub limit: usize,
}

impl Page<'_> {
    pub(in crate::search_service) fn validate(&self) -> Result<(), &'static str> {
        View {
            source_path: self.source_path,
            source_id: self.source_id,
            conversation_id: self.conversation_id,
            message_index: 1,
            context: 0,
        }
        .validate()?;
        if !(1..=MAX_MESSAGES).contains(&self.limit) {
            return Err("transcript limit must be between 1 and 32 message chunks");
        }
        if let Some(cursor) = self.cursor {
            cursor.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
enum Refusal {
    #[error("transcript cursor message identity or length changed; restart from a fresh page")]
    Changed,
    #[error("canonical transcript is not valid UTF-8 text or the cursor splits a code point")]
    InvalidContent,
}

pub(in crate::search_service) fn error_kind(error: &anyhow::Error) -> &'static str {
    match error.downcast_ref::<Refusal>() {
        Some(Refusal::Changed) => "canonical_cursor_changed",
        Some(Refusal::InvalidContent) => "canonical_invalid_content",
        None => super::error_kind(error),
    }
}

pub(in crate::search_service) fn read(db: &Path, page: &Page<'_>) -> Result<Value> {
    page.validate().map_err(anyhow::Error::msg)?;
    with_snapshot(db, |connection, started| {
        read_page(connection, page, started)
    })
}

fn anchors(connection: &Connection, page: &Page<'_>) -> Result<Vec<Anchor>> {
    let count = i64::try_from(page.limit + 1)?;
    let selected = if let Some(cursor) = page.cursor {
        let idx = i64::try_from(cursor.message_index - 1)?;
        connection.query_map_collect(
            "SELECT id, idx FROM messages WHERE conversation_id = ?1 AND idx >= ?2 ORDER BY idx LIMIT ?3",
            params![page.conversation_id, idx, count],
            |row| Ok(Anchor { id: row.get_typed(0)?, idx: row.get_typed(1)? }),
        )
    } else {
        // Do not filter out invalid negative indices on the first page.
        connection.query_map_collect(
            "SELECT id, idx FROM messages WHERE conversation_id = ?1 ORDER BY idx LIMIT ?2",
            params![page.conversation_id, count],
            |row| Ok(Anchor { id: row.get_typed(0)?, idx: row.get_typed(1)? }),
        )
    }
    .context("select bounded canonical transcript identities")?;
    if selected.len() > page.limit + 1
        || selected
            .iter()
            .any(|anchor| anchor.id <= 0 || anchor.idx < 0)
        || selected.windows(2).any(|pair| pair[0].idx >= pair[1].idx)
    {
        return Err(CanonicalRefusal::InvalidCoordinates.into());
    }
    if let Some(cursor) = page.cursor
        && selected.first().is_none_or(|anchor| {
            anchor.id != cursor.message_id || (anchor.idx as u64) + 1 != cursor.message_index
        })
    {
        // A deleted anchor must not silently select the next surviving message.
        return Err(Refusal::Changed.into());
    }
    Ok(selected)
}

struct Chunk {
    content: String,
    role: String,
    total_bytes: u64,
    end_byte: u64,
}

fn chunk(
    connection: &Connection,
    conversation: i64,
    anchor: Anchor,
    offset: u64,
    expected_bytes: Option<u64>,
    remaining: usize,
) -> Result<Chunk> {
    let start = i64::try_from(offset)?
        .checked_add(1)
        .context("transcript byte position overflow")?;
    let rows = connection.query_map_collect(
        "SELECT typeof(content), length(CAST(content AS BLOB)),
         CASE WHEN typeof(content) <> 'text' THEN NULL
              WHEN length(CAST(content AS BLOB)) = 0 THEN x''
              ELSE substr(CAST(content AS BLOB), ?4, ?5) END,
         CASE WHEN typeof(role) = 'text' AND length(CAST(role AS BLOB)) <= ?6 THEN role ELSE NULL END
         FROM messages WHERE conversation_id = ?1 AND id = ?2 AND idx = ?3 LIMIT 2",
        params![conversation, anchor.id, anchor.idx, start, i64::try_from(remaining)?, super::MAX_ROLE_BYTES as i64],
        |row| Ok((
            row.get_typed::<String>(0)?, row.get_typed::<Option<i64>>(1)?,
            row.get_typed::<Option<Vec<u8>>>(2)?, row.get_typed::<Option<String>>(3)?,
        )),
    ).context("read canonical transcript byte range")?;
    ensure!(
        rows.len() == 1,
        "canonical transcript identity changed within a read snapshot"
    );
    let (kind, length, bytes, role) = rows
        .into_iter()
        .next()
        .context("canonical transcript row missing")?;
    if kind != "text" {
        return Err(Refusal::InvalidContent.into());
    }
    let length = u64::try_from(length.context("canonical transcript byte length missing")?)?;
    if expected_bytes.is_some_and(|expected| expected != length)
        || offset > length
        || (offset != 0 && offset == length)
    {
        return Err(Refusal::Changed.into());
    }
    let mut bytes = bytes.context("canonical transcript byte range missing")?;
    let expected = (length - offset).min(u64::try_from(remaining)?);
    ensure!(
        u64::try_from(bytes.len())? == expected,
        "canonical transcript byte range has the wrong length"
    );
    if let Err(error) = std::str::from_utf8(&bytes) {
        // Only an incomplete *trailing* code point before EOF is deferred.
        // Invalid bytes, mid-code-point input cursors, and incomplete final
        // text are failures, never replaced with U+FFFD or dropped.
        if error.error_len().is_none() && offset + expected < length && error.valid_up_to() != 0 {
            bytes.truncate(error.valid_up_to());
        } else {
            return Err(Refusal::InvalidContent.into());
        }
    }
    let content = String::from_utf8(bytes).map_err(|_| Refusal::InvalidContent)?;
    let end_byte = offset
        .checked_add(u64::try_from(content.len())?)
        .context("transcript byte position overflow")?;
    let role = role.context("canonical role must be text of at most 128 bytes")?;
    Ok(Chunk {
        content,
        role: if role.eq_ignore_ascii_case("agent") {
            "assistant".into()
        } else {
            role
        },
        total_bytes: length,
        end_byte,
    })
}

fn read_page(connection: &Connection, page: &Page<'_>, started: Instant) -> Result<Value> {
    page.validate().map_err(anyhow::Error::msg)?;
    check_budget(started)?;
    require_conversation_identity(
        connection,
        page.source_path,
        page.source_id,
        page.conversation_id,
    )?;
    check_budget(started)?;
    let anchors = anchors(connection, page)?;
    let mut remaining = MAX_PAGE_BYTES;
    let mut messages = Vec::with_capacity(page.limit);
    let mut next = None;
    for (position, &anchor) in anchors.iter().enumerate() {
        // Four bytes ensure every legal UTF-8 code point can make progress.
        // Empty messages also count toward the independently bounded row limit.
        if position == page.limit || remaining < 4 {
            next = Some(Cursor::at(anchor));
            break;
        }
        check_budget(started)?;
        let cursor = page.cursor.filter(|_| position == 0);
        let offset = cursor.map_or(0, |cursor| cursor.byte_offset);
        let result = chunk(
            connection,
            page.conversation_id,
            anchor,
            offset,
            cursor.and_then(|cursor| cursor.content_bytes),
            remaining,
        )?;
        let content_bytes = result.content.len();
        remaining -= content_bytes;
        let complete = result.end_byte == result.total_bytes;
        messages.push(json!({
            "message_id": anchor.id, "message_index": (anchor.idx as u64) + 1,
            "role": result.role, "content": result.content,
            "byte_offset": offset, "end_byte_offset": result.end_byte,
            "content_bytes": content_bytes, "message_content_bytes": result.total_bytes,
            "message_complete": complete, "partial_message": offset != 0 || !complete,
        }));
        if !complete {
            next = Some(Cursor {
                byte_offset: result.end_byte,
                content_bytes: Some(result.total_bytes),
                ..Cursor::at(anchor)
            });
            break;
        }
    }
    check_budget(started)?;
    Ok(json!({
        "source_path": page.source_path, "source_id": page.source_id,
        "conversation_id": page.conversation_id,
        "messages": messages, "count": messages.len(), "limit": page.limit,
        "content_bytes": MAX_PAGE_BYTES - remaining,
        "next_cursor": next, "end_of_conversation": next.is_none(),
        "content_source": "canonical_archive", "content_scope": "message_byte_ranges",
        "snapshot_policy": "one_archive_read_transaction_per_page",
        "cross_page_snapshot": false, "cursor_validation": "message_identity_and_length_only",
        "matches_lexical_snapshot": null, "lexical_freshness": "not_checked",
        "preview_only": false, "maintenance_performed": false,
    }))
}

#[cfg(test)]
#[path = "transcript_tests.rs"]
mod tests;
