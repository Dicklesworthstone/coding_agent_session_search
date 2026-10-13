//! Uncapped semantic message rows without a conversation-sized result buffer.
//! This is a read-only cursor. Its consumer must not issue reentrant SQL or
//! transaction control; packet construction and identity comparison need neither.

use crate::franken_sync::{Connection, FrankenError, Row, SqliteValue, compat::RowExt};
use crate::model::types::{Message, MessageRole};
use anyhow::{Context, Result};

const INDEXED: &str = "SELECT id, idx, role, author, created_at, content
    FROM messages INDEXED BY sqlite_autoindex_messages_1
    WHERE conversation_id = ?1 ORDER BY idx";
const LEGACY: &str = "SELECT id, idx, role, author, created_at, content
    FROM messages WHERE conversation_id = ?1 ORDER BY idx";

fn message(row: &Row) -> Result<Message, FrankenError> {
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
        // Unlike lexical rebuilds, semantic identity requires the complete text.
        // Extra/raw-source envelopes and snippets do not enter this projection.
        content: row.get_typed(5)?,
        extra_json: serde_json::Value::Null,
        snippets: Vec::new(),
    })
}

pub(super) fn messages(
    connection: &Connection,
    conversation_id: i64,
    consume: &mut dyn FnMut(Message) -> Result<()>,
) -> Result<()> {
    let params = [SqliteValue::from(conversation_id)];
    let mut sql = INDEXED;
    loop {
        let mut saw_row = false;
        let mut consumer_error = None;
        let result = connection.query_with_params_for_each(sql, &params, |row| {
            saw_row = true;
            let decoded = message(row)?;
            if let Err(error) = consume(decoded) {
                // The engine callback accepts FrankenError, while the proof's
                // delta/cancellation markers are typed anyhow causes. Retain
                // the original cause and ask the cursor to finish its cleanup.
                consumer_error = Some(error);
                return Err(FrankenError::Abort);
            }
            Ok(())
        });
        if let Some(error) = consumer_error {
            return Err(error);
        }
        match result {
            Ok(_) => return Ok(()),
            Err(error)
                if sql == INDEXED
                    && !saw_row
                    && error.to_string().contains("no such index: sqlite_autoindex_messages_1") =>
            {
                // Only missing legacy schema metadata permits another query.
                // Never replay delivered rows or retry an arbitrary SQL error.
                sql = LEGACY;
            }
            Err(error) => {
                return Err(error).context("stream canonical semantic message rows");
            }
        }
    }
}

#[cfg(test)]
mod tests;
