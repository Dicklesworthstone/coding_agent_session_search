//! Direct semantic preparation shares reconciliation's bounded message reader.
//!
//! One integer per nonempty parent is retained for a stable progress denominator.
//! Message text is admitted in 64-row / 1-MiB pages, with one oversized message
//! allowed whole. Consumers can embed and release passages during replay instead
//! of retaining the corpus. Native/allocator scratch remains additional memory.

use super::{PARENT_PAGE_SIZE, engine, envelopes, pages};
use crate::franken_sync::{Connection, FrankenError, SqliteValue, compat::RowExt};
use crate::storage::sqlite::FrankenStorage;
use anyhow::{Context, Result, ensure};

struct ReadSnapshot<'a> {
    connection: &'a Connection,
    owned: bool,
}

impl<'a> ReadSnapshot<'a> {
    fn begin(connection: &'a Connection) -> Result<Self> {
        let owned = !connection.as_async().in_transaction();
        if owned {
            connection
                .execute("BEGIN DEFERRED")
                .context("begin direct semantic replay snapshot")?;
        }
        Ok(Self { connection, owned })
    }

    fn release(mut self) -> Result<()> {
        if self.owned {
            self.connection
                .execute("ROLLBACK")
                .context("release direct semantic replay snapshot")?;
            self.owned = false;
        }
        Ok(())
    }
}

impl Drop for ReadSnapshot<'_> {
    fn drop(&mut self) {
        if self.owned
            && let Err(error) = self.connection.execute("ROLLBACK")
        {
            tracing::warn!(%error, "failed to release direct semantic replay snapshot");
        }
    }
}

fn has_messages(connection: &Connection, parent: i64, indexed: &mut bool) -> Result<bool> {
    loop {
        let sql = if *indexed {
            "SELECT 1 FROM messages INDEXED BY sqlite_autoindex_messages_1 WHERE conversation_id = ?1 LIMIT 1"
        } else {
            "SELECT 1 FROM messages WHERE conversation_id = ?1 LIMIT 1"
        };
        let mut found = false;
        let result =
            connection.query_with_params_for_each(sql, &[SqliteValue::from(parent)], |_| {
                if found {
                    return Err(FrankenError::Internal(
                        "semantic parent probe exceeded its limit".into(),
                    ));
                }
                found = true;
                Ok(())
            });
        match result {
            Ok(_) => return Ok(found),
            Err(error)
                if *indexed
                    && !found
                    && error
                        .to_string()
                        .contains("no such index: sqlite_autoindex_messages_1") =>
            {
                *indexed = false;
            }
            Err(error) => return Err(error).context("probe canonical semantic replay parent"),
        }
    }
}

fn nonempty_parents(connection: &Connection) -> Result<Vec<i64>> {
    let mut parents = Vec::new();
    let mut cursor = None;
    let mut indexed = true;
    loop {
        let (sql, params) = match cursor {
            None => (
                "SELECT id FROM conversations ORDER BY id LIMIT ?1",
                vec![SqliteValue::from(PARENT_PAGE_SIZE as i64)],
            ),
            Some(id) => (
                "SELECT id FROM conversations WHERE id > ?1 ORDER BY id LIMIT ?2",
                vec![
                    SqliteValue::from(id),
                    SqliteValue::from(PARENT_PAGE_SIZE as i64),
                ],
            ),
        };
        let mut ids = Vec::with_capacity(PARENT_PAGE_SIZE);
        connection
            .query_with_params_for_each(sql, &params, |row| {
                let id: i64 = row.get_typed(0)?;
                if ids.len() >= PARENT_PAGE_SIZE
                    || ids
                        .last()
                        .copied()
                        .or(cursor)
                        .is_some_and(|previous| id <= previous)
                {
                    return Err(FrankenError::Internal(
                        "direct semantic parent page did not advance".into(),
                    ));
                }
                ids.push(id);
                Ok(())
            })
            .context("plan direct semantic replay parents")?;
        for &id in &ids {
            if has_messages(connection, id, &mut indexed)? {
                parents.push(id);
            }
        }
        if ids.len() < PARENT_PAGE_SIZE {
            return Ok(parents);
        }
        cursor = ids.last().copied();
    }
}

/// Collect the same canonical inputs as the direct engine path without retaining
/// whole histories. Repeated progress values mark actual message work inside a
/// large parent, not completed conversations. Only a completely read parent
/// advances the conversation count. Errors return no successful input prefix.
/// A caller-owned transaction remains owned by the caller on every exit path.
#[cfg(test)]
pub(crate) fn packet_embedding_inputs_from_storage_with_progress<F>(
    storage: &FrankenStorage,
    on_progress: F,
) -> Result<Vec<engine::EmbeddingInput>>
where
    F: FnMut(usize, usize),
{
    let mut inputs = Vec::new();
    visit_packet_embedding_inputs_from_storage_with_progress(
        storage,
        |input| {
            inputs.push(input);
            Ok(())
        },
        on_progress,
    )?;
    Ok(inputs)
}

/// Replay one snapshot into a fallible consumer. An error or unwind never
/// becomes successful end-of-input, so an unpublished writer can discard its
/// buffered prefix without replacing the selected generation.
pub(crate) fn visit_packet_embedding_inputs_from_storage_with_progress<F, P>(
    storage: &FrankenStorage,
    mut consume: F,
    mut on_progress: P,
) -> Result<Option<i64>>
where
    F: FnMut(engine::EmbeddingInput) -> Result<()>,
    P: FnMut(usize, usize),
{
    let snapshot = ReadSnapshot::begin(storage.raw())?;
    // A caller's progress callback can unwind. Running SQL cleanup during that
    // unwind poisons native database locks merely because their guards drop
    // while thread::panicking(). Finish cleanup outside the unwind, then resume
    // the identical panic; no callback failure becomes a successful input prefix.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if matches!(
            dotenvy::var("CASS_SEMANTIC_STREAM_REPLAY").as_deref(),
            Ok("0")
        ) {
            // Explicit diagnostic opt-out retains legacy materialization, but
            // binds its watermark and content to the same read snapshot.
            let last_message_id = storage.max_message_id()?;
            for input in engine::packet_embedding_inputs_from_storage_with_progress(
                storage,
                &mut on_progress,
            )? {
                consume(input)?;
            }
            Ok(last_message_id)
        } else {
            replay(storage, &mut consume, &mut on_progress)
        }
    }));
    match result {
        Ok(Ok(last_message_id)) => {
            snapshot.release()?;
            Ok(last_message_id)
        }
        Ok(Err(error)) => {
            drop(snapshot);
            Err(error)
        }
        Err(panic) => {
            drop(snapshot);
            std::panic::resume_unwind(panic)
        }
    }
}

fn replay(
    storage: &FrankenStorage,
    consume: &mut impl FnMut(engine::EmbeddingInput) -> Result<()>,
    on_progress: &mut impl FnMut(usize, usize),
) -> Result<Option<i64>> {
    let parents = nonempty_parents(storage.raw())?;
    let total = parents.len();
    let mut completed = 0usize;
    let mut last_message_id = None;
    on_progress(0, total);
    for ids in parents.chunks(PARENT_PAGE_SIZE) {
        for envelope in envelopes(storage, ids)? {
            let mut seen = false;
            let mut worked_rows = 0usize;
            let mut worked_bytes = 0usize;
            pages::messages(
                storage.raw(),
                envelope.context.conversation_id,
                &mut |message| {
                    seen = true;
                    if let Some(id) = message.id.filter(|id| *id > 0) {
                        last_message_id =
                            Some(last_message_id.map_or(id, |previous: i64| previous.max(id)));
                    }
                    worked_rows += 1;
                    worked_bytes = worked_bytes.saturating_add(message.content.len());
                    for input in envelope.project(message)? {
                        consume(input)?;
                    }
                    if worked_rows >= pages::MAX_ROWS || worked_bytes >= pages::MAX_CONTENT_BYTES {
                        on_progress(completed, total);
                        worked_rows = 0;
                        worked_bytes = 0;
                    }
                    Ok(())
                },
            )?;
            ensure!(
                seen,
                "planned semantic conversation lost its canonical messages"
            );
            completed += 1;
            on_progress(completed, total);
        }
    }
    Ok(last_message_id)
}
