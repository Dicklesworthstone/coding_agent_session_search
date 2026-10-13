//! Lazy canonical replay for a read-only unchanged proof.
//!
//! Page only parent IDs and envelopes. Load one parent's messages immediately
//! before visiting it, and stop before another body load on a delta/error/stop.
//! A whole conversation can still be large; this is not a byte or RSS ceiling.

use super::{Coverage, engine};
use crate::franken_sync::{SqliteValue, compat::{ConnectionExt, ParamValue, RowExt}};
use crate::model::conversation_packet::{ConversationPacket, ConversationPacketProvenance};
use crate::model::types::{Conversation, Message};
use crate::search::tantivy::{
    normalized_index_origin_host, normalized_index_origin_kind, normalized_index_source_id,
};
use crate::storage::sqlite::FrankenStorage;
use anyhow::{Context, Result, ensure};
use std::path::PathBuf;

const PAGE_SIZE: usize = 64;

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
    }).context("read unchanged-proof canonical envelopes")?;
    ensure!(
        rows.len() == ids.len() && rows.iter().zip(ids).all(|(row, id)| row.context.conversation_id == *id),
        "canonical parents changed while loading unchanged-proof envelopes"
    );
    Ok(rows)
}

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
    visit_with_loader(storage, |id| messages(storage, id), consume, check_cancelled)
}

fn visit_with_loader(
    storage: &FrankenStorage,
    mut load: impl FnMut(i64) -> Result<Vec<Message>>,
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
        storage.raw().query_with_params_for_each(sql, &params, |row| {
            ids.push(row.get_typed::<i64>(0)?);
            Ok(())
        }).context("page unchanged-proof canonical parents")?;
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
        for mut envelope in envelopes(storage, &ids)? {
            check_cancelled()?;
            envelope.conversation.messages = load(envelope.context.conversation_id)?;
            check_cancelled()?;
            if envelope.conversation.messages.is_empty() {
                continue;
            }
            // Count actual nonempty parents before semantic projection: blank
            // messages count, empty parents do not, and passage count is unrelated.
            coverage.0 = coverage.0.checked_add(1).context("canonical parent count overflow")?;
            coverage.1 = envelope.context.conversation_id;
            // Move message strings into the established canonical encoder,
            // rather than cloning a second full conversation. Use its semantic
            // projection/chunking/provenance, not a parallel document-ID format.
            let packet = ConversationPacket::from_canonical_replay_owned(
                envelope.conversation, envelope.provenance,
            );
            let inputs = engine::semantic_inputs_from_packets(
                std::slice::from_ref(&packet), std::slice::from_ref(&envelope.context),
            )?;
            drop(packet);
            for input in inputs {
                check_cancelled()?;
                consume(input)?;
            }
        }
        cursor = ids.last().copied();
        if ids.len() < PAGE_SIZE {
            check_cancelled()?;
            return Ok(coverage);
        }
    }
}

#[cfg(test)]
mod tests;
