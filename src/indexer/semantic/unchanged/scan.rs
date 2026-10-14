//! Read-only cache-miss work. Mismatches stop replay; successful proofs still
//! require exact nonempty-parent coverage and the caller's final file fences.

use super::engine;
#[cfg(test)]
use crate::franken_sync::{Connection, SqliteValue, compat::RowExt};
use crate::indexer::semantic_progress::{
    SemanticProgressEvent, SemanticProgressFields, SemanticProgressSink,
};
use crate::storage::sqlite::FrankenStorage;
use anyhow::{Context, Result};
use std::collections::HashSet;

#[cfg(test)]
const PARENT_PAGE_SIZE: usize = 128;
#[cfg(test)]
const FIRST_PARENTS: &str = "SELECT id FROM conversations ORDER BY id ASC LIMIT ?1";
#[cfg(test)]
const NEXT_PARENTS: &str = "SELECT id FROM conversations WHERE id > ?1 ORDER BY id ASC LIMIT ?2";
#[cfg(test)]
const INDEXED_MESSAGE: &str = "SELECT 1 FROM messages INDEXED BY sqlite_autoindex_messages_1
    WHERE conversation_id = ?1 LIMIT 1";
#[cfg(test)]
const LEGACY_MESSAGE: &str = "SELECT 1 FROM messages WHERE conversation_id = ?1 LIMIT 1";

#[derive(Debug, thiserror::Error)]
#[error("canonical identity differs from the retained semantic publication")]
struct CanonicalDelta;

/// Only this module's mismatch marker ends replay normally. SQL errors and
/// cancellation must propagate, not become a cache miss or a successful proof.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Comparison {
    pub(super) matches: bool,
    pub(super) visited: u64,
}

/// A complete count and maximum of canonical parents that contain messages.
pub(super) type Coverage = (u64, i64);

// Preserve the existing comparison regressions through the exact production
// implementation. The independent SQL coverage oracle below stays test-only.
#[cfg(test)]
pub(super) fn compare(
    storage: &FrankenStorage,
    remaining: &mut HashSet<&str>,
    sink: &SemanticProgressSink,
    rows_total: u64,
    check_cancelled: impl FnMut() -> Result<()>,
) -> Result<Comparison> {
    compare_with_coverage(storage, remaining, sink, rows_total, check_cancelled)
        .map(|(comparison, _)| comparison)
}

pub(super) fn compare_with_coverage(
    storage: &FrankenStorage,
    remaining: &mut HashSet<&str>,
    sink: &SemanticProgressSink,
    rows_total: u64,
    mut check_cancelled: impl FnMut() -> Result<()>,
) -> Result<(Comparison, Option<Coverage>)> {
    check_cancelled()?;
    let mut visited = 0u64;
    let replay = replay::visit(
        storage,
        |input| {
            if let Some(id) = engine::semantic_doc_id_for_input(&input) {
                visited = visited
                    .checked_add(1)
                    .context("semantic replay count overflow")?;
                // Removing instead of looking up also refuses duplicate canonical
                // identities. Lazy replay stops before loading another conversation
                // when this callback detects a delta.
                if !remaining.remove(id.as_str()) {
                    return Err(CanonicalDelta.into());
                }
                if visited.is_multiple_of(1024) {
                    sink.emit(
                        SemanticProgressEvent::PacketReplayProgress,
                        SemanticProgressFields {
                            rows_processed: Some(visited),
                            rows_total: Some(rows_total),
                            ..Default::default()
                        },
                    );
                }
            }
            Ok(())
        },
        &mut check_cancelled,
    );
    // Partial scans cannot supply coverage. Even a complete replay with
    // leftover retained IDs is a mismatch, not a count receipt for a no-op.
    let coverage = match replay {
        Ok(coverage) if remaining.is_empty() => Some(coverage),
        Ok(_) => None,
        Err(error) if error.is::<CanonicalDelta>() => None,
        Err(error) => return Err(error),
    };
    check_cancelled()?;
    Ok((
        Comparison {
            matches: coverage.is_some(),
            visited,
        },
        coverage,
    ))
}

/// Match COUNT(nonempty parents), COALESCE(MAX(nonempty parent id), 0)
/// without a correlated aggregate or retaining messages. Tail metadata is not
/// authoritative here: edits, deletions and legacy imports can leave it stale.
///
/// The production unique (conversation_id, idx) index makes each existence
/// probe independent of the conversation's body/history size. A legacy schema
/// missing that exact index uses the unhinted probe, never a guessed count.
/// This remains O(parents) probes, not a constant-time archive summary.
#[cfg(test)]
pub(super) fn nonempty_conversations(
    connection: &Connection,
    mut check_cancelled: impl FnMut() -> Result<()>,
) -> Result<(u64, i64)> {
    let mut cursor = None;
    let mut conversations = 0u64;
    let mut last_nonempty = 0i64;
    let mut indexed = true;
    loop {
        check_cancelled()?;
        // The first page has no lower bound: zero/negative SQLite rowids must
        // not disappear from a coverage proof. Subsequent pages do not add to
        // the cursor, so i64::MAX is safe too.
        let (sql, params) = match cursor {
            None => (
                FIRST_PARENTS,
                vec![SqliteValue::from(PARENT_PAGE_SIZE as i64)],
            ),
            Some(id) => (
                NEXT_PARENTS,
                vec![
                    SqliteValue::from(id),
                    SqliteValue::from(PARENT_PAGE_SIZE as i64),
                ],
            ),
        };
        let mut parents = Vec::with_capacity(PARENT_PAGE_SIZE);
        connection
            .query_with_params_for_each(sql, &params, |row| {
                parents.push(row.get_typed::<i64>(0)?);
                Ok(())
            })
            .context("page canonical parents for unchanged semantic coverage")?;
        if parents.is_empty() {
            break;
        }
        for &id in &parents {
            check_cancelled()?;
            let params = [SqliteValue::from(id)];
            let mut present = false;
            let result = connection.query_with_params_for_each(
                if indexed {
                    INDEXED_MESSAGE
                } else {
                    LEGACY_MESSAGE
                },
                &params,
                |_| {
                    present = true;
                    Ok(())
                },
            );
            if let Err(error) = result {
                if !indexed
                    || !error
                        .to_string()
                        .contains("no such index: sqlite_autoindex_messages_1")
                {
                    return Err(error).context("probe canonical messages for unchanged coverage");
                }
                // Remember the missing hint for this invocation. No other
                // storage error is swallowed or retried with a weaker query.
                indexed = false;
                present = false;
                check_cancelled()?;
                connection
                    .query_with_params_for_each(LEGACY_MESSAGE, &params, |_| {
                        present = true;
                        Ok(())
                    })
                    .context("probe legacy messages for unchanged coverage")?;
            }
            if present {
                conversations = conversations
                    .checked_add(1)
                    .context("parent count overflow")?;
                last_nonempty = id;
            }
        }
        cursor = parents.last().copied();
        if parents.len() < PARENT_PAGE_SIZE {
            break;
        }
    }
    check_cancelled()?;
    Ok((conversations, last_nonempty))
}

#[cfg(test)]
mod tests;

mod replay;
