//! Lazy canonical replay for a read-only unchanged proof.
//!
//! Page only parent IDs and envelopes, then stream each parent's message rows.
//! The proof retains one message's packet/passages, not a conversation's history.
//! One message and engine scratch can still be large; this is not an RSS ceiling.

use super::{Coverage, engine};
use crate::franken_sync::{SqliteValue, compat::RowExt};
use crate::indexer::semantic::replay::envelopes;
use crate::model::conversation_packet::ConversationPacket;
use crate::model::types::Message;
use crate::storage::sqlite::FrankenStorage;
use anyhow::{Context, Result, ensure};

const PAGE_SIZE: usize = 64;

#[cfg(test)]
fn messages(storage: &FrankenStorage, id: i64) -> Result<Vec<Message>> {
    // Use the SAME uncapped projection as engine replay. The lexical-only
    // single-conversation helper truncates content and cannot prove semantics.
    let mut grouped = storage.fetch_messages_for_lexical_rebuild_batch(&[id], None, None)?;
    Ok(grouped.remove(&id).unwrap_or_default())
}

pub(super) fn visit(
    storage: &FrankenStorage,
    consume: impl FnMut(engine::EmbeddingInput) -> Result<()>,
    check_cancelled: impl FnMut() -> Result<()>,
) -> Result<Coverage> {
    visit_with_reader(
        storage,
        |id, emit| stream::messages(storage.raw(), id, emit),
        consume,
        check_cancelled,
    )
}

// Keep the original loader regressions on the same packet/coverage path. Only
// this test adapter collects bodies; production never enters the Vec loader.
#[cfg(test)]
fn visit_with_loader(
    storage: &FrankenStorage,
    mut load: impl FnMut(i64) -> Result<Vec<Message>>,
    consume: impl FnMut(engine::EmbeddingInput) -> Result<()>,
    check_cancelled: impl FnMut() -> Result<()>,
) -> Result<Coverage> {
    visit_with_reader(
        storage,
        |id, emit| {
            for message in load(id)? {
                emit(message)?;
            }
            Ok(())
        },
        consume,
        check_cancelled,
    )
}

fn visit_with_reader(
    storage: &FrankenStorage,
    mut read: impl FnMut(i64, &mut dyn FnMut(Message) -> Result<()>) -> Result<()>,
    mut consume: impl FnMut(engine::EmbeddingInput) -> Result<()>,
    mut check_cancelled: impl FnMut() -> Result<()>,
) -> Result<Coverage> {
    let mut cursor = None;
    let mut coverage: Coverage = (0, 0);
    loop {
        check_cancelled()?;
        // No preliminary count/tail scan. No lower bound on the first page:
        // signed SQLite parent IDs and i64::MAX are valid, with no +1 overflow.
        let (sql, params) = match cursor {
            None => (
                "SELECT id FROM conversations ORDER BY id ASC LIMIT ?1",
                vec![SqliteValue::from(PAGE_SIZE as i64)],
            ),
            Some(id) => (
                "SELECT id FROM conversations WHERE id > ?1 ORDER BY id ASC LIMIT ?2",
                vec![SqliteValue::from(id), SqliteValue::from(PAGE_SIZE as i64)],
            ),
        };
        let mut ids = Vec::with_capacity(PAGE_SIZE);
        storage
            .raw()
            .query_with_params_for_each(sql, &params, |row| {
                ids.push(row.get_typed::<i64>(0)?);
                Ok(())
            })
            .context("page unchanged-proof canonical parents")?;
        if ids.is_empty() {
            check_cancelled()?;
            return Ok(coverage);
        }
        ensure!(
            ids.windows(2).all(|pair| pair[0] < pair[1])
                && cursor.is_none_or(|previous| ids[0] > previous),
            "unchanged-proof parent page did not advance strictly"
        );
        check_cancelled()?;
        for envelope in envelopes(storage, &ids)? {
            check_cancelled()?;
            let mut nonempty = false;
            read(envelope.context.conversation_id, &mut |message| {
                check_cancelled()?;
                nonempty = true;
                // The canonical semantic projection/chunking is per message.
                // Clone only the empty envelope; move this row's full text into
                // the existing packet encoder. No content truncation or second
                // identity format is introduced, and no SQL is issued here.
                let mut conversation = envelope.conversation.clone();
                conversation.messages.push(message);
                let packet = ConversationPacket::from_canonical_replay_owned(
                    conversation,
                    envelope.provenance.clone(),
                );
                let inputs = engine::semantic_inputs_from_packets(
                    std::slice::from_ref(&packet),
                    std::slice::from_ref(&envelope.context),
                )?;
                drop(packet);
                for input in inputs {
                    check_cancelled()?;
                    consume(input)?;
                    // A stop observed after an input must unwind the active
                    // cursor before it admits another row of this same history.
                    check_cancelled()?;
                }
                check_cancelled()
            })?;
            check_cancelled()?;
            if nonempty {
                // Blank/unsupported messages still make the parent nonempty.
                // Never return partial coverage after a consumer or SQL error.
                coverage.0 = coverage
                    .0
                    .checked_add(1)
                    .context("canonical parent count overflow")?;
                coverage.1 = envelope.context.conversation_id;
            }
        }
        cursor = ids.last().copied();
        if ids.len() < PAGE_SIZE {
            check_cancelled()?;
            return Ok(coverage);
        }
    }
}

mod stream;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod streaming_tests;
