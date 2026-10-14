//! Hydrate explicit semantic refreshes by message identity, not by history.
//!
//! Selection retains only integer coordinates. SQL bind lists and envelope
//! pages are bounded; body reads are primary-key lookups of accepted messages.
//! The returned inputs and selected coordinates still scale with the requested
//! delta, and one message stays whole. This is not a process memory ceiling.
//! Callers retain canonical snapshot/maintenance and publication authority.

use super::{PARENT_PAGE_SIZE, engine, envelopes, pages};
use crate::franken_sync::{Connection, FrankenError, SqliteValue, compat::RowExt};
use crate::model::types::Message;
use crate::storage::sqlite::FrankenStorage;
use anyhow::{Context, Result, ensure};
use std::collections::{BTreeMap, HashSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MessageKey {
    id: i64,
    idx: i64,
}

type Selection = BTreeMap<i64, Vec<MessageKey>>;

const BODY_SQL: &str = "SELECT id, idx, role, author, created_at, content
    FROM messages WHERE id = ?1 AND conversation_id = ?2 AND idx = ?3";

const HIGH_WATER_SQL: &str = "SELECT id FROM messages
    WHERE id > ?1 ORDER BY id DESC LIMIT 1";
const SINCE_SQL: &str = "SELECT id, conversation_id, idx FROM messages
    WHERE id > ?1 AND id <= ?2 ORDER BY id LIMIT ?3";

fn select_since(connection: &Connection, since_message_id: i64) -> Result<Selection> {
    let mut high_water = None;
    connection.query_with_params_for_each(
        HIGH_WATER_SQL,
        &[SqliteValue::from(since_message_id)],
        |row| {
            let id: i64 = row.get_typed(0)?;
            if id <= since_message_id || high_water.replace(id).is_some() {
                return Err(FrankenError::Internal("invalid semantic catch-up high water".into()));
            }
            Ok(())
        },
    ).context("capture canonical semantic catch-up high water")?;
    let Some(high_water) = high_water else {
        return Ok(Selection::new());
    };
    select_since_through(connection, since_message_id, high_water)
}

fn select_since_through(
    connection: &Connection,
    since_message_id: i64,
    high_water: i64,
) -> Result<Selection> {
    ensure!(high_water > since_message_id, "invalid semantic catch-up range");
    let mut selected = Selection::new();
    let mut cursor = since_message_id;
    while cursor < high_water {
        let mut count = 0;
        connection.query_with_params_for_each(
            SINCE_SQL,
            &[SqliteValue::from(cursor), SqliteValue::from(high_water),
                SqliteValue::from(pages::MAX_ROWS as i64)],
            |row| {
                let id: i64 = row.get_typed(0)?;
                if count >= pages::MAX_ROWS || id <= cursor || id > high_water {
                    return Err(FrankenError::Internal(
                        "semantic catch-up identity page did not advance within its range".into(),
                    ));
                }
                let parent: i64 = row.get_typed(1)?;
                let idx: i64 = row.get_typed(2)?;
                selected.entry(parent).or_default().push(MessageKey { id, idx });
                cursor = id;
                count += 1;
                Ok(())
            },
        ).context("page canonical semantic catch-up identities")?;
        // The captured endpoint must be observed. A disappearing endpoint
        // cannot turn an incomplete scan into an acknowledged catch-up batch.
        ensure!(count > 0, "canonical semantic catch-up endpoint disappeared");
    }
    Ok(selected)
}

fn selected_sql(count: usize) -> String {
    let placeholders = (1..=count)
        .map(|n| format!("?{n}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "SELECT id, conversation_id, idx FROM messages
         WHERE id IN ({placeholders}) ORDER BY id"
    )
}

fn select_ids(
    connection: &Connection,
    conversation_ids: &[i64],
    message_ids: &HashSet<i64>,
) -> Result<Selection> {
    let allowed: HashSet<i64> = conversation_ids.iter().copied().collect();
    let mut ids: Vec<i64> = message_ids.iter().copied().collect();
    ids.sort_unstable();
    let mut selected = Selection::new();
    if allowed.is_empty() {
        return Ok(selected);
    }
    for chunk in ids.chunks(pages::MAX_ROWS) {
        let params: Vec<_> = chunk.iter().copied().map(SqliteValue::from).collect();
        let mut previous = None;
        connection.query_with_params_for_each(&selected_sql(chunk.len()), &params, |row| {
            let id: i64 = row.get_typed(0)?;
            if chunk.binary_search(&id).is_err()
                || previous.is_some_and(|previous| id <= previous)
            {
                return Err(FrankenError::Internal(
                    "selected semantic identity query returned an unexpected row".into(),
                ));
            }
            previous = Some(id);
            let parent: i64 = row.get_typed(1)?;
            // Do not decode unrelated bodies, or even their order keys. A
            // caller-supplied message ID is not permission to widen its scope.
            if allowed.contains(&parent) {
                selected.entry(parent).or_default().push(MessageKey {
                    id,
                    idx: row.get_typed(2)?,
                });
            }
            Ok(())
        }).context("select canonical semantic refresh identities")?;
    }
    Ok(selected)
}

fn load_message(connection: &Connection, parent: i64, key: MessageKey) -> Result<Message> {
    let row = connection.query_row_with_params(
        BODY_SQL,
        &[SqliteValue::from(key.id), SqliteValue::from(parent), SqliteValue::from(key.idx)],
    ).context("load selected canonical semantic message")?;
    // Share the full replay's role/text decoder. Extra source envelopes and
    // snippets do not enter semantic identity construction.
    Ok(pages::decode(&row)?)
}

fn project_selection(
    storage: &FrankenStorage,
    selected: Selection,
    mut load: impl FnMut(i64, MessageKey) -> Result<Message>,
) -> Result<(Vec<engine::EmbeddingInput>, Option<i64>)> {
    let mut inputs = Vec::new();
    let mut raw_max_message_id: Option<i64> = None;
    let mut selected = selected.into_iter();
    loop {
        let group: Vec<_> = selected.by_ref().take(PARENT_PAGE_SIZE).collect();
        if group.is_empty() {
            break;
        }
        let parents: Vec<_> = group.iter().map(|(parent, _)| *parent).collect();
        // A missing admitted parent is a race/corruption error, not authority
        // to acknowledge its selected messages as successfully processed.
        for (envelope, (parent, mut keys)) in envelopes(storage, &parents)?.into_iter().zip(group) {
            keys.sort_unstable_by_key(|key| (key.idx, key.id));
            for key in keys {
                // No SQL cursor remains active here. Only this requested
                // message, not the rest of its conversation, is materialized.
                let message = load(parent, key)?;
                ensure!(message.id == Some(key.id) && message.idx == key.idx,
                    "canonical message identity changed during selected semantic replay");
                raw_max_message_id = Some(raw_max_message_id.map_or(key.id, |id| id.max(key.id)));
                inputs.extend(envelope.project(message)?);
            }
        }
    }
    Ok((inputs, raw_max_message_id))
}

/// Preserve the intersection of explicit parent/message scopes and canonical
/// parent/order-key ordering, while reading no unselected message bodies.
/// Missing requested IDs are allowed; failure after admission rejects the
/// entire result rather than returning a successful prefix.
pub(crate) fn packet_embedding_inputs_from_storage_for_message_ids(
    storage: &FrankenStorage,
    conversation_ids: &[i64],
    message_ids: &HashSet<i64>,
) -> Result<Vec<engine::EmbeddingInput>> {
    if matches!(dotenvy::var("CASS_SEMANTIC_STREAM_REPLAY").as_deref(), Ok("0")) {
        return engine::packet_embedding_inputs_from_storage_for_message_ids(
            storage, conversation_ids, message_ids,
        );
    }
    let selected = select_ids(storage.raw(), conversation_ids, message_ids)?;
    let (inputs, _) = project_selection(storage, selected, |parent, key| {
        load_message(storage.raw(), parent, key)
    })?;
    Ok(inputs)
}

/// Catch up only the raw message-ID delta. A captured high water bounds this
/// invocation; later appends remain for the next call. This bound is not a
/// snapshot lease: callers still serialize or snapshot canonical mutations.
/// Raw progress includes blank/non-embeddable messages, while emitted inputs
/// keep the existing canonical parent/(idx,id)/passage order.
pub(crate) fn packet_embedding_inputs_from_storage_since(
    storage: &FrankenStorage,
    since_message_id: i64,
) -> Result<engine::CanonicalIncrementalEmbeddingBatch> {
    if matches!(dotenvy::var("CASS_SEMANTIC_STREAM_REPLAY").as_deref(), Ok("0")) {
        return engine::packet_embedding_inputs_from_storage_since(storage, since_message_id);
    }
    let selected = select_since(storage.raw(), since_message_id)?;
    let conversations_in_batch = u64::try_from(selected.len()).unwrap_or(u64::MAX);
    let (inputs, raw_max_message_id) = project_selection(storage, selected, |parent, key| {
        load_message(storage.raw(), parent, key)
    })?;
    Ok(engine::CanonicalIncrementalEmbeddingBatch {
        inputs,
        conversations_in_batch,
        raw_max_message_id,
    })
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod since_tests;
