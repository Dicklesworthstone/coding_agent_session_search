//! Admit message bodies from bounded identity/size pages before loading text.
//!
//! No native cursor crosses an arbitrary consumer callback. The content budget
//! is exact for ordinary pages; a single oversized message is admitted whole.
//! Metadata, allocator overhead, native scratch and consumer outputs are extra.

use crate::franken_sync::{Connection, FrankenError, Row, SqliteValue, compat::RowExt};
use crate::model::types::{Message, MessageRole};
use anyhow::{Context, Result, ensure};

pub(super) const MAX_ROWS: usize = 64;
pub(super) const MAX_CONTENT_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug)]
pub(super) struct Limits {
    pub(super) rows: usize,
    pub(super) content_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self { rows: MAX_ROWS, content_bytes: MAX_CONTENT_BYTES }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Cursor {
    idx: i64,
    id: i64,
}

struct Identity {
    cursor: Cursor,
    content_bytes: usize,
}

struct Page {
    messages: Vec<Message>,
    last: Option<Cursor>,
    exhausted: bool,
}

pub(super) fn decode(row: &Row) -> Result<Message, FrankenError> {
    let role: String = row.get_typed(2)?;
    Ok(Message {
        id: Some(row.get_typed(0)?),
        idx: row.get_typed(1)?,
        role: match role.as_str() {
            "user" => MessageRole::User,
            "agent" | "assistant" => MessageRole::Agent,
            "tool" => MessageRole::Tool,
            "system" => MessageRole::System,
            other => MessageRole::Other(other.to_owned()),
        },
        author: row.get_typed(3)?,
        created_at: row.get_typed(4)?,
        content: row.get_typed(5)?,
        extra_json: serde_json::Value::Null,
        snippets: Vec::new(),
    })
}

fn query(cursor: Option<Cursor>, indexed: bool, projection: &str) -> String {
    let hint = if indexed { " INDEXED BY sqlite_autoindex_messages_1" } else { "" };
    // idx >= provides the composite-index lower bound. The second predicate
    // keeps a stable id tie-break for permissive legacy schemas. No cursor+1
    // arithmetic, OFFSET, or conversation-wide body collection is needed.
    let after = if cursor.is_some() {
        " AND idx >= ?2 AND (idx > ?2 OR id > ?3)"
    } else {
        ""
    };
    let limit = if cursor.is_some() { "?4" } else { "?2" };
    format!(
        "SELECT {projection} FROM messages{hint}
         WHERE conversation_id = ?1{after} ORDER BY idx, id LIMIT {limit}"
    )
}

fn read_page(
    connection: &Connection,
    conversation_id: i64,
    cursor: Option<Cursor>,
    indexed: &mut bool,
    limits: Limits,
) -> Result<Page> {
    ensure!((1..=MAX_ROWS).contains(&limits.rows), "invalid semantic replay row limit");
    ensure!((1..=MAX_CONTENT_BYTES).contains(&limits.content_bytes),
        "invalid semantic replay content budget");
    let mut params = vec![SqliteValue::from(conversation_id)];
    if let Some(cursor) = cursor {
        params.extend([SqliteValue::from(cursor.idx), SqliteValue::from(cursor.id)]);
    }
    params.push(SqliteValue::from(limits.rows as i64));
    // The byte count is UTF-8 bytes, not Unicode code points. Only IDs, order
    // keys and sizes leave SQLite during admission, not rejected message text.
    let identities = loop {
        let mut identities = Vec::with_capacity(limits.rows);
        let mut last = cursor;
        let mut saw_row = false;
        let sql = query(cursor, *indexed, "id, idx, length(CAST(content AS BLOB))");
        let result = connection.query_with_params_for_each(&sql, &params, |row| {
            saw_row = true;
            if identities.len() >= limits.rows {
                return Err(FrankenError::Internal("semantic size page exceeded its row limit".into()));
            }
            let key = Cursor { id: row.get_typed(0)?, idx: row.get_typed(1)? };
            if last.is_some_and(|last| key <= last) {
                return Err(FrankenError::Internal("semantic message page did not advance".into()));
            }
            let bytes: i64 = row.get_typed(2)?;
            let bytes = usize::try_from(bytes).map_err(|_| {
                FrankenError::Internal("invalid semantic message content length".into())
            })?;
            identities.push(Identity { cursor: key, content_bytes: bytes });
            last = Some(key);
            Ok(())
        });
        match result {
            Ok(_) => break identities,
            Err(error) if *indexed && !saw_row
                && error.to_string().contains("no such index: sqlite_autoindex_messages_1") =>
            {
                *indexed = false;
            }
            Err(error) => return Err(error).context("read canonical semantic message sizes"),
        }
    };
    let mut count = 0usize;
    let mut bytes = 0usize;
    for identity in &identities {
        if count > 0 && (bytes >= limits.content_bytes
            || identity.content_bytes > limits.content_bytes.saturating_sub(bytes))
        {
            break;
        }
        count += 1;
        bytes = bytes.saturating_add(identity.content_bytes);
    }
    if count == 0 {
        return Ok(Page { messages: Vec::new(), last: None, exhausted: true });
    }
    let exhausted = count == identities.len() && identities.len() < limits.rows;
    let mut messages = Vec::with_capacity(count);
    let limit_index = params.len() - 1;
    params[limit_index] = SqliteValue::from(count as i64);
    connection.query_with_params_for_each(
        &query(cursor, *indexed, "id, idx, role, author, created_at, content"),
        &params,
        |row| {
            if messages.len() >= count {
                return Err(FrankenError::Internal("semantic body page exceeded admission".into()));
            }
            let message = decode(row)?;
            let observed = Cursor { id: row.get_typed(0)?, idx: message.idx };
            let Some(expected) = identities.get(messages.len()).filter(|_| messages.len() < count) else {
                return Err(FrankenError::Internal("semantic body page exceeded admission".into()));
            };
            if observed != expected.cursor || message.content.len() != expected.content_bytes {
                return Err(FrankenError::Internal("semantic message changed during body admission".into()));
            }
            messages.push(message);
            Ok(())
        },
    ).context("read admitted canonical semantic message bodies")?;
    ensure!(messages.len() == count, "canonical semantic messages disappeared during body admission");
    Ok(Page { messages, last: Some(identities[count - 1].cursor), exhausted })
}

pub(super) fn messages(
    connection: &Connection,
    conversation_id: i64,
    consume: &mut dyn FnMut(Message) -> Result<()>,
) -> Result<()> {
    messages_with_limits(connection, conversation_id, Limits::default(), consume)
}

fn messages_with_limits(
    connection: &Connection,
    conversation_id: i64,
    limits: Limits,
    consume: &mut dyn FnMut(Message) -> Result<()>,
) -> Result<()> {
    let mut cursor = None;
    let mut indexed = true;
    loop {
        let Page { messages, last, exhausted } =
            read_page(connection, conversation_id, cursor, &mut indexed, limits)?;
        // Both SELECTs have finished. Consumer code can query the same storage
        // handle, and typed consumer failures are never SQL/fallback signals.
        for message in messages {
            consume(message)?;
        }
        if exhausted || last.is_none() {
            return Ok(());
        }
        cursor = last;
    }
}

#[cfg(test)]
mod tests;
