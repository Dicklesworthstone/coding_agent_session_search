//! Bounded canonical follow-ups for answer packs.

use std::collections::{HashMap, HashSet};

use anyhow::Result;

use super::{
    FieldMask, MatchType, RankedMessageHydrationCandidate, SearchClient, SearchFilters, SearchHit,
    SearchReadTransaction, semantic_message_id_from_db,
};
use crate::franken_sync::compat::{ParamValue, RowExt};

const MAX_CONTEXT_ANCHORS: usize = 32;
const FOLLOWING_MESSAGE_WINDOW: i64 = 1;
const MAX_CONTEXT_BODY_BYTES: i64 = 128 * 1024;

struct ContextRow {
    message_id: u64,
    index: i64,
    role: String,
    content_bytes: i64,
}

impl SearchClient {
    /// Load the immediately following archived message for each admitted hit.
    /// This supplies context, not additional text-search matches: callers
    /// must not attach lexical/semantic scores or search ranks to these rows.
    ///
    /// Exact anchor identity and content are rechecked in the same read
    /// transaction as follow-up hydration. A stale index row, a shared provider
    /// pathname, or a reused message ordinal cannot attach another session's
    /// answer. Every result also passes the original search filters.
    pub(crate) fn pack_answer_context(
        &self,
        anchors: &[SearchHit],
        filters: &SearchFilters,
    ) -> Result<Vec<(SearchHit, String)>> {
        let privacy = self.lexical_admission_snapshot()?;
        let outcome = (|| {
            if anchors.is_empty() {
                return Ok(Vec::new());
            }
            let guard = self.sqlite_guard()?;
            let Some(connection) = guard.as_ref() else {
                return Ok(Vec::new());
            };
            let mut snapshot = SearchReadTransaction::begin(connection)?;
            let mut seen_anchors = HashSet::new();
            let mut seen_messages = HashSet::new();
            let mut context = Vec::new();

            for anchor in anchors.iter().take(MAX_CONTEXT_ANCHORS) {
                let Some(conversation_id) = anchor.conversation_id else {
                    continue;
                };
                let Some(index) = anchor
                    .line_number
                    .and_then(|line| line.checked_sub(1))
                    .and_then(|index| i64::try_from(index).ok())
                else {
                    continue;
                };
                if anchor.content.is_empty()
                    || anchor.content.len() > MAX_CONTEXT_BODY_BYTES as usize
                    || seen_anchors.contains(&(conversation_id, index))
                    || !Self::sqlite_fts5_hit_matches_filters(anchor, filters)
                {
                    continue;
                }

                // Read metadata before bodies so an oversized tool result
                // cannot materialize unbounded context. Retain oversized user
                // rows here: they still terminate the originating turn.
                let rows: Vec<ContextRow> = snapshot.query_map_collect(
                    "SELECT id, idx, COALESCE(role, ''),
                            COALESCE(length(CAST(content AS BLOB)), 0)
                     FROM messages INDEXED BY sqlite_autoindex_messages_1
                     WHERE conversation_id = ? AND idx >= ? AND idx <= ?
                     ORDER BY idx LIMIT 2",
                    &[
                        ParamValue::from(conversation_id),
                        ParamValue::from(index),
                        ParamValue::from(index.saturating_add(FOLLOWING_MESSAGE_WINDOW)),
                    ],
                    |row| {
                        Ok(ContextRow {
                            message_id: semantic_message_id_from_db(row.get_typed(0)?)?,
                            index: row.get_typed(1)?,
                            role: row.get_typed(2)?,
                            content_bytes: row.get_typed(3)?,
                        })
                    },
                )?;
                let Some(first) = rows.first().filter(|row| {
                    row.index == index && row.content_bytes <= MAX_CONTEXT_BODY_BYTES
                }) else {
                    continue;
                };
                let anchor_message_id = first.message_id;
                let mut wanted = Vec::with_capacity(rows.len());
                let mut roles = HashMap::new();
                for row in rows {
                    if row.index > index && row.role.eq_ignore_ascii_case("user") {
                        break;
                    }
                    if row.content_bytes > MAX_CONTEXT_BODY_BYTES {
                        continue;
                    }
                    wanted.push(RankedMessageHydrationCandidate {
                        message_id: row.message_id,
                        score: 0.0,
                    });
                    roles.insert(row.message_id, row.role);
                }
                let hydrated = self.hydrate_ranked_message_hits_in_transaction(
                    &snapshot,
                    &wanted,
                    FieldMask::FULL,
                    MatchType::Exact,
                )?;
                let Some((_, current_anchor)) = hydrated
                    .iter()
                    .find(|(message_id, _)| *message_id == anchor_message_id)
                else {
                    continue;
                };
                if !same_pack_anchor(anchor, current_anchor) {
                    continue;
                }
                seen_anchors.insert((conversation_id, index));

                for (message_id, hit) in hydrated {
                    if message_id == anchor_message_id
                        || !Self::sqlite_fts5_hit_matches_filters(&hit, filters)
                        || !seen_messages.insert(message_id)
                    {
                        continue;
                    }
                    if let Some(role) = roles.remove(&message_id) {
                        context.push((hit, role));
                    }
                }
            }
            snapshot.rollback()?;
            Ok(context)
        })();
        // A concurrent forget must remain an authoritative refusal even if
        // archive hydration failed or no bounded follow-ups were available.
        Self::validate_lexical_privacy(&privacy)?;
        outcome
    }
}

fn same_pack_anchor(expected: &SearchHit, current: &SearchHit) -> bool {
    expected.conversation_id == current.conversation_id
        && expected.line_number == current.line_number
        && expected.source_id == current.source_id
        && expected.source_path == current.source_path
        && expected.agent == current.agent
        && expected.workspace == current.workspace
        && expected.origin_kind == current.origin_kind
        && expected.origin_host == current.origin_host
        && expected.created_at == current.created_at
        && expected.title == current.title
        && expected.content_hash == current.content_hash
        && expected.content == current.content
}
