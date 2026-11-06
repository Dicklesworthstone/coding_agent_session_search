//! Bounded native maintenance of the optional conversation-title channel.
//!
//! The canonical mutation journal is read inside one pinned transaction. A
//! batch acknowledges only the rows it captured; writes that occur while the
//! embedder runs remain for a later batch. Bootstrap uses a conversation-ID
//! cursor and then replays changes since its starting authority, so title edits
//! behind that cursor are not missed. No query calls this module.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use super::engine::SemanticIndexer;
use crate::franken_sync::compat::{RowExt, Transaction, TransactionExt};
use crate::franken_sync::{Row, SqliteValue};
use crate::search::canonicalize::content_hash;
use crate::search::semantic_coverage::semantic_file_sha256;
use crate::search::semantic_manifest::sync_parent_directory;
use crate::search::title_index::{
    TITLE_MAX_PARTITION_RECORDS, TITLE_SHARD_MAX_BYTES, TitleDocId, TitleIndexManifest,
    TitleJournalCursor, TitleShard, load_title_manifest, normalized_title, publish_title_manifest,
    read_title_partition, title_identity_hash, title_index_root, title_model_revision,
    title_partition, title_shard_name, title_vector_revision, title_writer_lease,
};
use crate::search::vector_index::{Quantization, VectorIndex};
use crate::storage::sqlite::{FrankenStorage, SemanticArchiveState};

/// Explicit builds may choose this ceiling; routine callers use smaller
/// batches. One partition is loaded at a time, at most 4,096 title vectors.
pub const TITLE_MAX_BATCH_CONVERSATIONS: usize = 10_000;

const TITLE_SCOPE_INDEX: &str = "idx_messages_title_scope";
// `substr` bounds archive text before allocating it on the Rust side. Keep
// this literal in lockstep with TITLE_MAX_SCAN_CHARS (tested below).
const TITLE_ROW_PROJECTION: &str = "SELECT c.id, COALESCE(c.agent_id, 0), c.workspace_id, \
     COALESCE(c.source_id, 'local'), c.external_id, c.source_path, c.started_at, \
     substr(c.title, 1, 512), COALESCE((SELECT j.invalidation_seq \
     FROM semantic_conversation_changes j WHERE j.conversation_id = c.id), 0) \
     FROM conversations c";

#[derive(Debug, Clone, Serialize)]
pub struct TitleMaintenanceOutcome {
    pub complete: bool,
    pub published: bool,
    pub unchanged: bool,
    pub examined_conversations: usize,
    pub embedded_titles: usize,
    pub reused_titles: usize,
    pub removed_titles: usize,
    pub rewritten_partitions: usize,
    pub copied_vectors: usize,
    pub indexed_titles: usize,
    pub live_bytes: u64,
    pub archive_mutation_seq: u64,
    pub title_mutation_seq: u64,
    pub bootstrap_after: Option<i64>,
    pub journal_cursor: TitleJournalCursor,
}

impl TitleMaintenanceOutcome {
    fn from_manifest(manifest: &TitleIndexManifest, current: &SemanticArchiveState) -> Self {
        Self {
            complete: manifest.complete && manifest.archive_state == *current,
            published: false,
            unchanged: false,
            examined_conversations: 0,
            embedded_titles: 0,
            reused_titles: 0,
            removed_titles: 0,
            rewritten_partitions: 0,
            copied_vectors: 0,
            indexed_titles: manifest.indexed_titles,
            live_bytes: manifest.live_bytes(),
            archive_mutation_seq: current.mutation_seq,
            title_mutation_seq: manifest.archive_state.mutation_seq,
            bootstrap_after: manifest.bootstrap_after,
            journal_cursor: manifest.journal_cursor.clone(),
        }
    }
}

/// One-time, explicit opt-in setup. Building this SQLite index may scan all
/// messages; routine maintenance and searches only verify that it exists.
/// The caller must supply a writable connection outside any read snapshot.
pub fn prepare_title_scope_index(storage: &FrankenStorage) -> Result<bool> {
    let present = title_scope_index_present(storage)?;
    if !present {
        storage.raw().execute(
            "CREATE INDEX IF NOT EXISTS idx_messages_title_scope \
             ON messages(conversation_id, role, created_at, id)",
        )?;
    }
    ensure_title_scope_index(storage)?;
    Ok(!present)
}

fn title_scope_index_present(storage: &FrankenStorage) -> Result<bool> {
    Ok(!storage
        .raw()
        .query_with_params(
            "SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = ?1",
            &[SqliteValue::from(TITLE_SCOPE_INDEX)],
        )?
        .is_empty())
}

fn ensure_title_scope_index(storage: &FrankenStorage) -> Result<()> {
    ensure!(
        title_scope_index_present(storage)?,
        "title scope index is missing; run explicit semantic indexing to prepare title retrieval"
    );
    let columns = storage
        .raw()
        .query("PRAGMA index_info(idx_messages_title_scope)")?
        .into_iter()
        .map(|row| row.get_typed::<String>(2))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(
        columns == ["conversation_id", "role", "created_at", "id"],
        "title scope index has an incompatible definition"
    );
    Ok(())
}

/// Constant bounded metadata preflight. In particular a weight/policy change
/// does not construct an embedder, walk titles, or rewrite title artifacts.
pub fn title_maintenance_needed(
    storage: &FrankenStorage,
    data_dir: &Path,
    embedder_id: &str,
) -> Result<bool> {
    let Some(manifest) = load_title_manifest(data_dir, embedder_id)? else {
        return Ok(true);
    };
    let current = storage.semantic_archive_state()?;
    Ok(!manifest.complete
        || !manifest.compatible(embedder_id, 384)
        || current.as_ref() != Some(&manifest.archive_state)
        || !title_scope_index_present(storage)?)
}

struct TitlePacket {
    conversation_id: i64,
    value: Option<(TitleDocId, String)>,
}

struct CapturedTitles {
    manifest: TitleIndexManifest,
    current: SemanticArchiveState,
    packets: Vec<TitlePacket>,
}

fn archive_state(transaction: &Transaction<'_>) -> Result<SemanticArchiveState> {
    let rows =
        transaction.query("SELECT value FROM meta WHERE key = 'semantic_archive_state_v1'")?;
    let row = rows
        .first()
        .context("title indexing requires canonical archive mutation authority")?;
    SemanticArchiveState::parse(&row.get_typed::<String>(0)?)
}

fn representative_message(
    transaction: &Transaction<'_>,
    conversation_id: i64,
) -> Result<Option<i64>> {
    // This fixed number of indexed point/range lookups does not scan a long
    // conversation. Prefer default searchable roles to make normal query
    // admission a single batched canonical witness read.
    for role in ["user", "assistant", "agent", "system", "tool"] {
        let rows = transaction.query_with_params(
            "SELECT id FROM messages INDEXED BY idx_messages_title_scope \
             WHERE conversation_id = ?1 AND role = ?2 \
             ORDER BY created_at, id LIMIT 1",
            &[SqliteValue::from(conversation_id), SqliteValue::from(role)],
        )?;
        if let Some(row) = rows.first() {
            return Ok(Some(row.get_typed(0)?));
        }
    }
    // Future canonical roles still have a real citation; the query's role
    // filter decides whether that witness is eligible.
    let rows = transaction.query_with_params(
        "SELECT id FROM messages INDEXED BY idx_messages_title_scope \
         WHERE conversation_id = ?1 LIMIT 1",
        &[SqliteValue::from(conversation_id)],
    )?;
    rows.first()
        .map(|row| row.get_typed(0).map_err(Into::into))
        .transpose()
}

fn title_packet(transaction: &Transaction<'_>, row: &Row) -> Result<TitlePacket> {
    let conversation_id: i64 = row.get_typed(0)?;
    ensure!(
        conversation_id > 0,
        "invalid canonical title conversation ID"
    );
    let raw_title: Option<String> = row.get_typed(7)?;
    let Some(title) = normalized_title(raw_title.as_deref()) else {
        return Ok(TitlePacket {
            conversation_id,
            value: None,
        });
    };
    let Some(representative_message_id) = representative_message(transaction, conversation_id)?
    else {
        // A conversation without messages cannot supply a trustworthy view or
        // pack citation and must not become a semantic result.
        return Ok(TitlePacket {
            conversation_id,
            value: None,
        });
    };
    let agent_id = row.get_typed(1)?;
    let workspace_id = row.get_typed(2)?;
    let source_id: String = row.get_typed(3)?;
    let external_id: Option<String> = row.get_typed(4)?;
    let source_path: String = row.get_typed(5)?;
    let started_at = row.get_typed(6)?;
    let invalidation_seq = u64::try_from(row.get_typed::<i64>(8)?)?;
    let identity = TitleDocId {
        conversation_id,
        representative_message_id,
        invalidation_seq,
        title_hash: content_hash(&title),
        identity_hash: title_identity_hash(
            conversation_id,
            agent_id,
            workspace_id,
            &source_id,
            external_id.as_deref(),
            &source_path,
            started_at,
        ),
    };
    Ok(TitlePacket {
        conversation_id,
        value: Some((identity, title)),
    })
}

fn capture_titles(
    engine: &SemanticIndexer,
    storage: &FrankenStorage,
    previous: Option<&TitleIndexManifest>,
    maximum: usize,
) -> Result<CapturedTitles> {
    let mut transaction = storage.raw().transaction()?;
    let captured = capture_titles_in_transaction(engine, &transaction, previous, maximum);
    // End the canonical snapshot before expensive inference/filesystem work.
    // An error still discharges the transaction's rollback obligation.
    let rollback = transaction.rollback();
    let captured = captured?;
    rollback?;
    Ok(captured)
}

fn capture_titles_in_transaction(
    engine: &SemanticIndexer,
    transaction: &Transaction<'_>,
    previous: Option<&TitleIndexManifest>,
    maximum: usize,
) -> Result<CapturedTitles> {
    let current = archive_state(transaction)?;
    let mut manifest = match previous {
        Some(previous)
            if previous.compatible(engine.embedder_id(), engine.embedder_dimension())
                && previous.archive_state.archive_id == current.archive_id
                && previous.journal_cursor.mutation_seq <= current.mutation_seq =>
        {
            previous.clone()
        }
        _ => TitleIndexManifest::fresh(
            engine.embedder_id(),
            engine.embedder_dimension(),
            current.clone(),
        )?,
    };
    let global = transaction.query(
        "SELECT mutation_seq FROM semantic_conversation_changes WHERE conversation_id = 0",
    )?;
    if let Some(row) = global.first()
        && u64::try_from(row.get_typed::<i64>(0)?)? > manifest.archive_state.mutation_seq
    {
        // The journal cannot describe this rewrite. A fresh bounded bootstrap
        // drops all old shard authority without touching body artifacts.
        manifest = TitleIndexManifest::fresh(
            engine.embedder_id(),
            engine.embedder_dimension(),
            current.clone(),
        )?;
    }
    let mut packets = Vec::new();
    let limit = i64::try_from(
        maximum
            .checked_add(1)
            .context("title batch budget overflow")?,
    )?;
    if let Some(after) = manifest.bootstrap_after {
        let rows = transaction.query_with_params(
            &format!("{TITLE_ROW_PROJECTION} WHERE c.id > ?1 ORDER BY c.id LIMIT ?2"),
            &[SqliteValue::from(after), SqliteValue::from(limit)],
        )?;
        let exhausted = rows.len() <= maximum;
        for row in rows.iter().take(maximum) {
            let packet = title_packet(transaction, row)?;
            manifest.bootstrap_after = Some(packet.conversation_id);
            packets.push(packet);
        }
        if exhausted {
            manifest.bootstrap_after = None;
            manifest.complete = manifest.archive_state == current;
        }
    } else {
        let cursor = &manifest.journal_cursor;
        // Two disjoint composite-index seeks avoid repeatedly scanning the
        // already consumed prefix of a large same-sequence transaction. An
        // OR predicate across both cursor components can lose that bound.
        let mut rows = transaction.query_with_params(
            "SELECT conversation_id, mutation_seq FROM semantic_conversation_changes \
             WHERE mutation_seq = ?1 AND conversation_id > ?2 \
             AND mutation_seq <= ?3 ORDER BY mutation_seq, conversation_id LIMIT ?4",
            &[
                SqliteValue::from(i64::try_from(cursor.mutation_seq)?),
                SqliteValue::from(cursor.conversation_id),
                SqliteValue::from(i64::try_from(current.mutation_seq)?),
                SqliteValue::from(limit),
            ],
        )?;
        if rows.len() < maximum + 1 {
            let remaining = limit - i64::try_from(rows.len())?;
            rows.extend(transaction.query_with_params(
                "SELECT conversation_id, mutation_seq FROM semantic_conversation_changes \
                 WHERE mutation_seq > ?1 AND mutation_seq <= ?2 \
                 ORDER BY mutation_seq, conversation_id LIMIT ?3",
                &[
                    SqliteValue::from(i64::try_from(cursor.mutation_seq)?),
                    SqliteValue::from(i64::try_from(current.mutation_seq)?),
                    SqliteValue::from(remaining),
                ],
            )?);
        }
        let exhausted = rows.len() <= maximum;
        for row in rows.iter().take(maximum) {
            let conversation_id: i64 = row.get_typed(0)?;
            ensure!(
                conversation_id > 0,
                "global title invalidation escaped bootstrap admission"
            );
            let mutation_seq = u64::try_from(row.get_typed::<i64>(1)?)?;
            let canonical = transaction.query_with_params(
                &format!("{TITLE_ROW_PROJECTION} WHERE c.id = ?1"),
                &[SqliteValue::from(conversation_id)],
            )?;
            packets.push(match canonical.first() {
                Some(row) => title_packet(transaction, row)?,
                None => TitlePacket {
                    conversation_id,
                    value: None,
                },
            });
            manifest.journal_cursor = TitleJournalCursor {
                mutation_seq,
                conversation_id,
            };
            // A partially drained transaction can contain many conversation
            // rows with the same sequence. Never acknowledge that whole
            // sequence until every row in it has been handled.
            manifest.archive_state.mutation_seq = mutation_seq.saturating_sub(1);
        }
        manifest.complete = exhausted;
        if exhausted {
            manifest.archive_state = current.clone();
            manifest.journal_cursor = TitleJournalCursor {
                mutation_seq: current.mutation_seq,
                conversation_id: i64::MAX,
            };
        }
    }
    Ok(CapturedTitles {
        manifest,
        current,
        packets,
    })
}

fn write_partition(
    engine: &SemanticIndexer,
    data_dir: &Path,
    partition: u16,
    slot: u8,
    records: &BTreeMap<i64, (TitleDocId, Vec<f32>)>,
) -> Result<TitleShard> {
    ensure!(
        !records.is_empty() && records.len() <= TITLE_MAX_PARTITION_RECORDS,
        "title partition capacity exceeded; the previous artifact remains selected"
    );
    let root = title_index_root(data_dir, engine.embedder_id())?;
    let path = root.join(title_shard_name(partition, slot)?);
    if let Ok(metadata) = fs::symlink_metadata(&path) {
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "title shard destination is not a regular file"
        );
    }
    let revision = title_vector_revision(engine.embedder_id())?;
    let mut writer = VectorIndex::create_with_revision(
        &path,
        engine.embedder_id(),
        &revision,
        engine.embedder_dimension(),
        Quantization::F16,
    )?;
    for (identity, vector) in records.values() {
        ensure!(
            vector.len() == engine.embedder_dimension()
                && vector.iter().all(|value| value.is_finite()),
            "invalid retained title vector"
        );
        writer.write_record(&identity.encode(), vector)?;
    }
    writer.finish()?;
    // Native writer atomically replaces the inactive slot. Validate it through
    // the production reader before selecting it in the title manifest.
    let index = VectorIndex::open_read_only(&path)?;
    ensure!(
        index.record_count() == records.len() && index.wal_record_count() == 0,
        "title shard publication count mismatch"
    );
    let mut observed = BTreeMap::new();
    for record in 0..index.record_count() {
        let identity = TitleDocId::parse(index.doc_id_at(record)?)
            .context("published title shard has an invalid identity")?;
        let expected = records
            .get(&identity.conversation_id)
            .context("published title shard contains an unexpected conversation")?;
        ensure!(expected.0 == identity, "published title witness mismatch");
        ensure!(
            observed.insert(identity.conversation_id, ()).is_none(),
            "published title identity is duplicated"
        );
        let vector = index.vector_at_f32(record)?;
        ensure!(
            vector.len() == engine.embedder_dimension()
                && vector.iter().all(|value| value.is_finite()),
            "published title vector is invalid"
        );
    }
    let mut file = File::open(&path)?;
    file.sync_all()?;
    let bytes = file.metadata()?.len();
    ensure!(
        bytes <= TITLE_SHARD_MAX_BYTES,
        "title shard exceeded its byte budget"
    );
    let sha256 = semantic_file_sha256(&mut file)?;
    sync_parent_directory(&path)?;
    Ok(TitleShard {
        slot,
        records: records.len(),
        bytes,
        sha256,
    })
}

/// Maintain at most `max_conversations` canonical rows, with a hard ceiling.
/// This neither downloads a model nor modifies body embeddings or history.
/// A partial result is resumable by calling the same function again.
pub fn maintain_title_index(
    indexer: &super::SemanticIndexer,
    storage: &FrankenStorage,
    data_dir: &Path,
    max_conversations: usize,
) -> Result<TitleMaintenanceOutcome> {
    let engine = &indexer.inner;
    ensure!(
        (1..=TITLE_MAX_BATCH_CONVERSATIONS).contains(&max_conversations),
        "title maintenance batch must be in 1..=10000 conversations"
    );
    ensure!(
        title_model_revision(engine.embedder_id()).is_some(),
        "title indexing requires a native or hash embedder"
    );
    ensure_title_scope_index(storage)?;
    let _lease = title_writer_lease(data_dir, engine.embedder_id())?;
    let previous = load_title_manifest(data_dir, engine.embedder_id())?;
    if let Some(previous) = &previous
        && previous.complete
        && previous.compatible(engine.embedder_id(), engine.embedder_dimension())
        && storage.semantic_archive_state()?.as_ref() == Some(&previous.archive_state)
    {
        let mut outcome = TitleMaintenanceOutcome::from_manifest(previous, &previous.archive_state);
        outcome.unchanged = true;
        return Ok(outcome);
    }
    let CapturedTitles {
        mut manifest,
        current,
        packets,
    } = capture_titles(engine, storage, previous.as_ref(), max_conversations)?;
    let mut outcome = TitleMaintenanceOutcome::from_manifest(&manifest, &current);
    outcome.examined_conversations = packets.len();
    let mut partitions = BTreeMap::<u16, Vec<TitlePacket>>::new();
    for packet in packets {
        partitions
            .entry(title_partition(packet.conversation_id)?)
            .or_default()
            .push(packet);
    }
    for (partition, packets) in partitions {
        let mut records = read_title_partition(data_dir, &manifest, partition)?;
        let mut changed = false;
        let mut pending = Vec::new();
        for packet in packets {
            match packet.value {
                None => {
                    if records.remove(&packet.conversation_id).is_some() {
                        changed = true;
                        outcome.removed_titles += 1;
                    }
                }
                Some((identity, title)) => {
                    if let Some((old_identity, _)) = records.get_mut(&packet.conversation_id)
                        && old_identity.title_hash == identity.title_hash
                    {
                        changed |= *old_identity != identity;
                        *old_identity = identity;
                        outcome.reused_titles += 1;
                    } else {
                        pending.push((identity, title));
                    }
                }
            }
        }
        let new_ids = pending
            .iter()
            .filter(|(identity, _)| !records.contains_key(&identity.conversation_id))
            .count();
        ensure!(
            records.len().saturating_add(new_ids) <= TITLE_MAX_PARTITION_RECORDS,
            "title partition reached the 4096-record limit; previous generation retained"
        );
        if !pending.is_empty() {
            let inputs = pending
                .iter()
                .map(|(_, title)| title.as_str())
                .collect::<Vec<_>>();
            let vectors = engine.embed_title_texts(&inputs)?;
            outcome.embedded_titles += vectors.len();
            for ((identity, _), vector) in pending.into_iter().zip(vectors) {
                records.insert(identity.conversation_id, (identity, vector));
            }
            changed = true;
        }
        if !changed {
            continue;
        }
        outcome.rewritten_partitions += 1;
        if records.is_empty() {
            manifest.shards.remove(&partition);
            continue;
        }
        // Even a representation/archive reset must avoid overwriting a slot
        // selected by the previous manifest until the new one is durable.
        let slot = previous
            .as_ref()
            .and_then(|old| old.shards.get(&partition))
            .map_or(0, |old| old.slot ^ 1);
        outcome.copied_vectors += records.len();
        let shard = write_partition(engine, data_dir, partition, slot, &records)?;
        manifest.shards.insert(partition, shard);
    }
    manifest.indexed_titles = manifest.shards.values().map(|shard| shard.records).sum();
    manifest.generation = format!("{:032x}", rand::random::<u128>());
    publish_title_manifest(data_dir, &manifest, previous.as_ref())?;
    outcome.published = true;
    outcome.indexed_titles = manifest.indexed_titles;
    outcome.live_bytes = manifest.live_bytes();
    outcome.complete = manifest.complete && manifest.archive_state == current;
    outcome.title_mutation_seq = manifest.archive_state.mutation_seq;
    outcome.bootstrap_after = manifest.bootstrap_after;
    outcome.journal_cursor = manifest.journal_cursor;
    Ok(outcome)
}
