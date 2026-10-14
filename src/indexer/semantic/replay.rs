//! Canonical semantic replay without retaining whole conversation histories.
//!
//! Parents/envelopes are paged separately from message bodies. Only a bounded
//! message page and one message's canonical packet/passages are retained here.
//! Consumer-owned outputs, large metadata, engine scratch and one oversized
//! message are not bounded by the content budget. Callers still own archive
//! snapshot/maintenance leases; this read-only visitor does not publish or mark
//! coverage durable. All SQL has returned before arbitrary consumer code runs.

use super::engine;
use crate::franken_sync::{SqliteValue, compat::{ConnectionExt, ParamValue, RowExt}};
use crate::model::conversation_packet::{ConversationPacket, ConversationPacketProvenance};
use crate::model::types::{Conversation, Message};
use crate::search::tantivy::{
    normalized_index_origin_host, normalized_index_origin_kind, normalized_index_source_id,
};
use crate::storage::sqlite::FrankenStorage;
use anyhow::{Context, Result, ensure};
use std::path::PathBuf;

mod pages;

const PARENT_PAGE_SIZE: usize = 64;

struct Envelope {
    conversation: Conversation,
    context: engine::SemanticPacketContext,
    provenance: ConversationPacketProvenance,
}

fn envelopes(storage: &FrankenStorage, ids: &[i64]) -> Result<Vec<Envelope>> {
    let placeholders = (1..=ids.len()).map(|n| format!("?{n}")).collect::<Vec<_>>().join(", ");
    let sql = format!(
        "SELECT c.id, COALESCE(a.slug, 'unknown'), COALESCE(c.agent_id, 0),
                c.workspace_id, w.path, c.external_id, c.title, c.source_path,
                c.started_at, c.ended_at, c.source_id, c.origin_host
         FROM conversations c
         LEFT JOIN agents a ON a.id = c.agent_id
         LEFT JOIN workspaces w ON w.id = c.workspace_id
         WHERE c.id IN ({placeholders}) ORDER BY c.id ASC"
    );
    let params = ids.iter().copied().map(ParamValue::from).collect::<Vec<_>>();
    let rows = storage.raw().query_map_collect(&sql, &params, |row| {
        let conversation_id: i64 = row.get_typed(0)?;
        let agent_id: i64 = row.get_typed(2)?;
        let workspace_id: Option<i64> = row.get_typed(3)?;
        let workspace: Option<String> = row.get_typed(4)?;
        let source_id: Option<String> = row.get_typed(10)?;
        let origin_host: Option<String> = row.get_typed(11)?;
        let source_id = normalized_index_source_id(source_id.as_deref(), None, origin_host.as_deref());
        let provenance = ConversationPacketProvenance {
            origin_kind: normalized_index_origin_kind(&source_id, None),
            origin_host: normalized_index_origin_host(origin_host.as_deref()),
            source_id,
        };
        Ok(Envelope {
            context: engine::SemanticPacketContext {
                conversation_id,
                agent_id: engine::saturating_u32_from_i64(agent_id),
                workspace_id: engine::saturating_u32_from_i64(workspace_id.unwrap_or(0)),
            },
            conversation: Conversation {
                id: Some(conversation_id),
                agent_slug: row.get_typed(1)?,
                workspace: workspace.map(PathBuf::from),
                external_id: row.get_typed(5)?,
                title: row.get_typed(6)?,
                source_path: PathBuf::from(row.get_typed::<String>(7)?),
                started_at: row.get_typed(8)?,
                ended_at: row.get_typed(9)?,
                approx_tokens: None,
                metadata_json: serde_json::Value::Null,
                messages: Vec::new(),
                source_id: provenance.source_id.clone(),
                origin_host: provenance.origin_host.clone(),
            },
            provenance,
        })
    }).context("read canonical semantic replay envelopes")?;
    ensure!(rows.len() == ids.len()
        && rows.iter().zip(ids).all(|(row, id)| row.context.conversation_id == *id),
        "canonical parents changed while loading semantic replay envelopes");
    Ok(rows)
}

/// Existing reconciliation entry point. Explicit facade re-export replaces the
/// conversation-sized engine visitor without changing its signature. Set
/// CASS_SEMANTIC_STREAM_REPLAY=0 for the retained legacy path when diagnosing
/// a native-engine regression. No optimization is allowed to truncate content.
pub(crate) fn visit_packet_embedding_inputs_from_storage<F>(
    storage: &FrankenStorage,
    consume: F,
) -> Result<()>
where
    F: FnMut(engine::EmbeddingInput) -> Result<()>,
{
    if matches!(dotenvy::var("CASS_SEMANTIC_STREAM_REPLAY").as_deref(), Ok("0")) {
        return engine::visit_packet_embedding_inputs_from_storage(storage, consume);
    }
    visit(storage, consume, || Ok(()))
}

fn visit(
    storage: &FrankenStorage,
    mut consume: impl FnMut(engine::EmbeddingInput) -> Result<()>,
    mut check_cancelled: impl FnMut() -> Result<()>,
) -> Result<()> {
    let mut cursor = None;
    loop {
        check_cancelled()?;
        let (sql, params) = match cursor {
            None => (
                "SELECT id FROM conversations ORDER BY id ASC LIMIT ?1",
                vec![SqliteValue::from(PARENT_PAGE_SIZE as i64)],
            ),
            Some(id) => (
                "SELECT id FROM conversations WHERE id > ?1 ORDER BY id ASC LIMIT ?2",
                vec![SqliteValue::from(id), SqliteValue::from(PARENT_PAGE_SIZE as i64)],
            ),
        };
        let mut ids = Vec::with_capacity(PARENT_PAGE_SIZE);
        storage.raw().query_with_params_for_each(sql, &params, |row| {
            ids.push(row.get_typed::<i64>(0)?);
            Ok(())
        }).context("page canonical semantic replay parents")?;
        if ids.is_empty() {
            return check_cancelled();
        }
        ensure!(ids.windows(2).all(|pair| pair[0] < pair[1])
            && cursor.is_none_or(|previous| ids[0] > previous),
            "canonical semantic replay parent page did not advance");
        check_cancelled()?;
        for envelope in envelopes(storage, &ids)? {
            check_cancelled()?;
            pages::messages(storage.raw(), envelope.context.conversation_id, &mut |message: Message| {
                check_cancelled()?;
                let mut conversation = envelope.conversation.clone();
                conversation.messages.push(message);
                let packet = ConversationPacket::from_canonical_replay_owned(
                    conversation, envelope.provenance.clone(),
                );
                let inputs = engine::semantic_inputs_from_packets(
                    std::slice::from_ref(&packet), std::slice::from_ref(&envelope.context),
                )?;
                drop(packet);
                for input in inputs {
                    check_cancelled()?;
                    consume(input)?;
                    check_cancelled()?;
                }
                check_cancelled()
            })?;
            check_cancelled()?;
        }
        cursor = ids.last().copied();
        if ids.len() < PARENT_PAGE_SIZE {
            return check_cancelled();
        }
    }
}

#[cfg(test)]
mod tests;
