//! Replayable, read-only archive input for daemon embedding.
//!
//! Canonical identity planning and embedding read the same SQLite snapshot.
//! Only a row/byte-bounded page of message bodies is retained at a time. A
//! single oversized message is admitted alone, never truncated. The engine's
//! own query buffers and the worker's vectors/identity sets are separate costs.

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::franken_sync::{
    Connection, SqliteValue,
    compat::{OpenFlags, RowExt, open_with_flags},
};
use crate::storage::sqlite::MessageForEmbedding;

const MAX_PAGE_ROWS: usize = 128;
const MAX_PAGE_BODY_BYTES: usize = 8 * 1024 * 1024;

pub(super) trait EmbeddingMessageSource {
    fn total_docs(&self) -> usize;

    /// Returns false on cancellation. A visitor failure is an error, never a
    /// successful short scan. Cancellation is checked before each page and row.
    fn visit(
        &self,
        cancelled: &dyn Fn() -> bool,
        visitor: &mut dyn FnMut(&MessageForEmbedding) -> Result<()>,
    ) -> Result<bool>;
}

pub(super) struct SqliteEmbeddingSource {
    connection: Connection,
    total_docs: usize,
    page_rows: usize,
    page_body_bytes: usize,
}

struct MessagePage {
    last_id: i64,
    messages: Vec<MessageForEmbedding>,
}

impl SqliteEmbeddingSource {
    pub(super) fn open(path: &Path) -> Result<Self> {
        let connection = open_with_flags(
            path.to_string_lossy().as_ref(),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .context("opening read-only daemon embedding snapshot")?;
        connection
            .execute("BEGIN DEFERRED TRANSACTION")
            .context("starting daemon embedding read snapshot")?;
        // This also pins the snapshot before progress writes on the separate
        // writer connection. Keep the JOIN: orphan messages were not eligible
        // in fetch_messages_for_embedding and must not inflate job totals.
        let count = connection
            .query_row(
                "SELECT COUNT(*) FROM messages m \
                 JOIN conversations c ON m.conversation_id = c.id",
            )?
            .get_typed::<i64>(0)?;
        let total_docs = usize::try_from(count).context("invalid embedding message count")?;
        Ok(Self {
            connection,
            total_docs,
            page_rows: MAX_PAGE_ROWS,
            page_body_bytes: MAX_PAGE_BODY_BYTES,
        })
        // Connection's synchronous Drop releases the read transaction on every
        // exit path, including constructor failure, cancellation and errors.
    }

    fn page_after(&self, after: Option<i64>) -> Result<Option<MessagePage>> {
        let limit = i64::try_from(self.page_rows)?;
        let (sql, params) = match after {
            Some(id) => (
                "SELECT id, COALESCE(LENGTH(CAST(content AS BLOB)), 0) \
                 FROM messages WHERE id > ?1 ORDER BY id LIMIT ?2",
                vec![SqliteValue::Integer(id), SqliteValue::Integer(limit)],
            ),
            None => (
                "SELECT id, COALESCE(LENGTH(CAST(content AS BLOB)), 0) \
                 FROM messages ORDER BY id LIMIT ?1",
                vec![SqliteValue::Integer(limit)],
            ),
        };
        let headers = self.connection.query_with_params(sql, &params)?;
        let mut last_id = None;
        let mut planned_bytes = 0usize;
        let mut selected_rows = 0usize;
        for row in headers {
            let id = row.get_typed::<i64>(0)?;
            let bytes = usize::try_from(row.get_typed::<i64>(1)?)
                .context("invalid embedding message byte length")?;
            if after.is_some_and(|previous| id <= previous)
                || last_id.is_some_and(|previous| id <= previous)
            {
                bail!("embedding page did not advance in canonical message order");
            }
            let next_bytes = planned_bytes.saturating_add(bytes);
            if selected_rows > 0 && next_bytes > self.page_body_bytes {
                break;
            }
            planned_bytes = next_bytes;
            last_id = Some(id);
            selected_rows += 1;
            if planned_bytes >= self.page_body_bytes {
                break;
            }
        }
        let Some(last_id) = last_id else {
            return Ok(None);
        };
        // The first page has no lower bound so even i64::MIN is visited and
        // classified consistently with the old whole-archive projection.
        let (sql, params) = match after {
            Some(id) => (
                "SELECT m.id, m.created_at, COALESCE(c.agent_id, 0), \
                 c.workspace_id, c.source_id, m.role, m.content \
                 FROM messages m JOIN conversations c ON m.conversation_id = c.id \
                 WHERE m.id > ?1 AND m.id <= ?2 ORDER BY m.id",
                vec![SqliteValue::Integer(id), SqliteValue::Integer(last_id)],
            ),
            None => (
                "SELECT m.id, m.created_at, COALESCE(c.agent_id, 0), \
                 c.workspace_id, c.source_id, m.role, m.content \
                 FROM messages m JOIN conversations c ON m.conversation_id = c.id \
                 WHERE m.id <= ?1 ORDER BY m.id",
                vec![SqliteValue::Integer(last_id)],
            ),
        };
        let mut messages = Vec::with_capacity(selected_rows);
        self.connection
            .query_with_params_for_each(sql, &params, |row| {
                let source_id = row
                    .get_typed::<Option<String>>(4)?
                    .unwrap_or_else(|| "local".to_string());
                messages.push(MessageForEmbedding {
                    message_id: row.get_typed(0)?,
                    created_at: row.get_typed(1)?,
                    agent_id: row.get_typed(2)?,
                    workspace_id: row.get_typed(3)?,
                    source_id_hash: crc32fast::hash(source_id.as_bytes()),
                    role: row.get_typed(5)?,
                    content: row.get_typed(6)?,
                });
                Ok(())
            })?;
        let actual_bytes = messages.iter().fold(0usize, |sum, message| {
            sum.saturating_add(message.content.len())
        });
        // Orphans may make the joined page smaller, but never larger. Both
        // queries share the pinned snapshot, including concurrent WAL writes.
        if messages.len() > selected_rows || actual_bytes > planned_bytes {
            bail!("embedding page exceeded its planned row/body-byte bounds");
        }
        Ok(Some(MessagePage { last_id, messages }))
    }
}

impl EmbeddingMessageSource for SqliteEmbeddingSource {
    fn total_docs(&self) -> usize {
        self.total_docs
    }

    fn visit(
        &self,
        cancelled: &dyn Fn() -> bool,
        visitor: &mut dyn FnMut(&MessageForEmbedding) -> Result<()>,
    ) -> Result<bool> {
        let mut after = None;
        let mut visited = 0usize;
        loop {
            if cancelled() {
                return Ok(false);
            }
            let Some(page) = self.page_after(after)? else {
                break;
            };
            after = Some(page.last_id);
            for message in page.messages {
                if cancelled() {
                    return Ok(false);
                }
                visitor(&message)?;
                visited = visited
                    .checked_add(1)
                    .context("embedding row count overflow")?;
            }
        }
        if visited != self.total_docs {
            bail!(
                "embedding snapshot row count changed: expected {}, visited {visited}",
                self.total_docs
            );
        }
        Ok(!cancelled())
    }
}

#[cfg(test)]
pub(super) struct SliceEmbeddingSource<'a>(pub(super) &'a [MessageForEmbedding]);

#[cfg(test)]
impl EmbeddingMessageSource for SliceEmbeddingSource<'_> {
    fn total_docs(&self) -> usize {
        self.0.len()
    }

    fn visit(
        &self,
        cancelled: &dyn Fn() -> bool,
        visitor: &mut dyn FnMut(&MessageForEmbedding) -> Result<()>,
    ) -> Result<bool> {
        for message in self.0 {
            if cancelled() {
                return Ok(false);
            }
            visitor(message)?;
        }
        Ok(!cancelled())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn fixture() -> Result<(tempfile::TempDir, Connection)> {
        let temp = tempfile::tempdir()?;
        let connection = Connection::open(temp.path().join("archive.db").to_string_lossy())?;
        connection.execute("PRAGMA journal_mode=WAL")?;
        connection.execute_batch(
            "CREATE TABLE conversations (id INTEGER PRIMARY KEY, agent_id INTEGER, \
             workspace_id INTEGER, source_id TEXT); \
             CREATE TABLE messages (id INTEGER PRIMARY KEY, conversation_id INTEGER, \
             created_at INTEGER, role TEXT, content TEXT); \
             INSERT INTO conversations VALUES (1, NULL, NULL, NULL), (2, 7, 9, 'remote');",
        )?;
        Ok((temp, connection))
    }

    fn insert(connection: &Connection, id: i64, conversation: i64, text: &str) -> Result<()> {
        connection.execute_with_params(
            "INSERT INTO messages VALUES (?1, ?2, 1700000000000, 'assistant', ?3)",
            &[
                SqliteValue::Integer(id),
                SqliteValue::Integer(conversation),
                SqliteValue::Text(text.to_string().into()),
            ],
        )?;
        Ok(())
    }

    #[test]
    fn pages_preserve_projection_sparse_ids_and_legacy_nulls() -> Result<()> {
        let (temp, writer) = fixture()?;
        insert(
            &writer,
            i64::MIN,
            1,
            "invalid ID remains visible to the worker",
        )?;
        insert(&writer, 0, 1, "Unicode café")?;
        insert(&writer, 9, 999, "orphan is not eligible")?;
        insert(&writer, i64::MAX, 2, "remote message")?;
        let mut source = SqliteEmbeddingSource::open(&temp.path().join("archive.db"))?;
        source.page_rows = 1;
        assert_eq!(source.total_docs(), 3);
        for _ in 0..2 {
            let mut values = Vec::new();
            assert!(source.visit(&|| false, &mut |message| {
                values.push((
                    message.message_id,
                    message.agent_id,
                    message.workspace_id,
                    message.source_id_hash,
                    message.content.clone(),
                ));
                Ok(())
            })?);
            assert_eq!(
                values,
                vec![
                    (
                        i64::MIN,
                        0,
                        None,
                        crc32fast::hash(b"local"),
                        "invalid ID remains visible to the worker".into()
                    ),
                    (0, 0, None, crc32fast::hash(b"local"), "Unicode café".into()),
                    (
                        i64::MAX,
                        7,
                        Some(9),
                        crc32fast::hash(b"remote"),
                        "remote message".into()
                    ),
                ]
            );
        }
        Ok(())
    }

    #[test]
    fn page_body_budget_counts_utf8_bytes_and_admits_one_oversized_row() -> Result<()> {
        let (temp, writer) = fixture()?;
        for (id, text) in [
            (1, "éé"),
            (2, "abcd"),
            (3, "oversized message"),
            (4, "tail"),
        ] {
            insert(&writer, id, 1, text)?;
        }
        let mut source = SqliteEmbeddingSource::open(&temp.path().join("archive.db"))?;
        source.page_body_bytes = 8;
        let first = source.page_after(None)?.context("first page")?;
        assert_eq!(first.last_id, 2);
        assert_eq!(first.messages.len(), 2);
        let second = source
            .page_after(Some(first.last_id))?
            .context("oversized page")?;
        assert_eq!(second.last_id, 3);
        assert_eq!(second.messages.len(), 1);
        assert_eq!(second.messages[0].content, "oversized message");
        let third = source
            .page_after(Some(second.last_id))?
            .context("tail page")?;
        assert_eq!(third.messages[0].content, "tail");
        assert!(source.page_after(Some(third.last_id))?.is_none());
        Ok(())
    }

    #[test]
    fn replay_uses_one_read_snapshot_while_writer_commits() -> Result<()> {
        let (temp, writer) = fixture()?;
        insert(&writer, 1, 1, "before")?;
        let path = temp.path().join("archive.db");
        let source = SqliteEmbeddingSource::open(&path)?;
        writer.execute("UPDATE messages SET content = 'after' WHERE id = 1")?;
        insert(&writer, 2, 2, "new row")?;
        for _ in 0..2 {
            let mut bodies = Vec::new();
            assert!(source.visit(&|| false, &mut |message| {
                bodies.push(message.content.clone());
                Ok(())
            })?);
            assert_eq!(bodies, ["before"]);
        }
        assert!(source.connection.execute("DELETE FROM messages").is_err());
        drop(source);
        let reopened = SqliteEmbeddingSource::open(&path)?;
        assert_eq!(reopened.total_docs(), 2);
        assert_eq!(
            reopened
                .page_after(None)?
                .context("reopened page")?
                .messages[0]
                .content,
            "after"
        );
        Ok(())
    }

    #[test]
    fn cancellation_and_visitor_failure_stop_without_losing_replayability() -> Result<()> {
        let (temp, writer) = fixture()?;
        for id in 1..=4 {
            insert(&writer, id, 1, "body")?;
        }
        let mut source = SqliteEmbeddingSource::open(&temp.path().join("archive.db"))?;
        source.page_rows = 1;
        let stopped = Cell::new(false);
        let mut seen = Vec::new();
        assert!(!source.visit(&|| stopped.get(), &mut |message| {
            seen.push(message.message_id);
            stopped.set(true);
            Ok(())
        })?);
        assert_eq!(seen, [1]);
        assert!(
            source
                .visit(&|| false, &mut |_| bail!("visitor failure"))
                .is_err()
        );
        let mut replay = Vec::new();
        assert!(source.visit(&|| false, &mut |message| {
            replay.push(message.message_id);
            Ok(())
        })?);
        assert_eq!(replay, [1, 2, 3, 4]);
        Ok(())
    }

    #[test]
    fn absent_database_is_not_created() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("missing.db");
        assert!(SqliteEmbeddingSource::open(&path).is_err());
        assert!(!path.exists());
        Ok(())
    }
}
