//! A completed-build cache is a skip hint, not permission to rewrite serving
//! assets. On a cache miss, prove a zero canonical delta before entering the
//! engine's mutating reconciliation. No model is run and no cache is trusted
//! as canonical-content evidence here.
//!
//! This is limited to current, ready legacy artifacts on Unix, where descriptor
//! identity and ctime can fence a read-only proof. Other formats/platforms keep
//! the existing reconciliation path. Existing ANN/readiness records are never
//! promoted or repaired by this optimization.

use super::engine::{self, SemanticBackfillBatchOutcome, SemanticBackfillStoragePlan};
use crate::franken_sync::compat::{ConnectionExt, ParamValue, RowExt};
use crate::indexer::semantic_progress::{
    SemanticProgressEvent, SemanticProgressFields, SemanticProgressSink,
};
use crate::search::policy::{CHUNKING_STRATEGY_VERSION, SEMANTIC_SCHEMA_VERSION};
use crate::search::semantic_manifest::{SemanticCurrentPointerV1, SemanticManifest, TierKind};
use crate::search::vector_index::vector_index_path;
use crate::storage::sqlite::FrankenStorage;
use anyhow::{Context, Result, ensure};
use frankensearch::index::{Quantization, VectorIndex, wal_path_for};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub(super) fn try_retain_completed(
    engine: &engine::SemanticIndexer,
    storage: &FrankenStorage,
    data_dir: &Path,
    manifest: &SemanticManifest,
    plan: &SemanticBackfillStoragePlan,
    sink: &SemanticProgressSink,
) -> Result<Option<SemanticBackfillBatchOutcome>> {
    // Keep the existing constant-time cache-hit path; do not turn every
    // scheduled no-op into a full canonical scan.
    if engine
        .completed_backfill_fingerprint(
            storage,
            data_dir,
            manifest,
            plan.tier,
            &plan.model_revision,
        )?
        .as_deref()
        == Some(plan.db_fingerprint.as_str())
    {
        return Ok(None);
    }
    prove_unchanged(engine, storage, data_dir, manifest, plan, sink, || Ok(()))
}

fn prove_unchanged<F>(
    engine: &engine::SemanticIndexer,
    storage: &FrankenStorage,
    data_dir: &Path,
    manifest: &SemanticManifest,
    plan: &SemanticBackfillStoragePlan,
    sink: &SemanticProgressSink,
    checkpoint: F,
) -> Result<Option<SemanticBackfillBatchOutcome>>
where
    F: FnOnce() -> Result<()>,
{
    if manifest.checkpoint.is_some() || plan.max_conversations == 0 {
        return Ok(None);
    }
    // Immutable selections require their owner-backed publication contract,
    // not a legacy ledger optimization.
    let pointer_path = SemanticCurrentPointerV1::path(data_dir);
    if stamp(&pointer_path)?.is_some() {
        return Ok(None);
    }
    let artifact = match plan.tier {
        TierKind::Fast => manifest.fast_tier.as_ref(),
        TierKind::Quality => manifest.quality_tier.as_ref(),
    };
    let Some(artifact) = artifact else {
        return Ok(None);
    };
    let path = vector_index_path(data_dir, engine.embedder_id());
    if !artifact.ready
        || artifact.tier != plan.tier
        || artifact.embedder_id != engine.embedder_id()
        || artifact.dimension != engine.embedder_dimension()
        || artifact.model_revision != plan.model_revision
        || artifact.schema_version != SEMANTIC_SCHEMA_VERSION
        || artifact.chunking_version != CHUNKING_STRATEGY_VERSION
        || artifact.db_fingerprint != plan.db_fingerprint
        || data_dir.join(&artifact.index_path) != path
    {
        return Ok(None);
    }
    let Some(archive_before) = ArchiveStamp::capture(storage)? else {
        return Ok(None);
    };
    let manifest_path = SemanticManifest::path(data_dir);
    let manifest_before = stamp(&manifest_path)?;
    let vector_before = stamp(&path)?;
    let Some(vector_stamp) = &vector_before else {
        return Ok(None);
    };
    if !vector_stamp.regular || vector_stamp.len != artifact.size_bytes {
        return Ok(None);
    }
    // Even a stale/empty WAL is unfinished maintenance, not a zero-delta
    // publication. Let the existing writer handle it without modifying it here.
    let wal = wal_path_for(&path);
    if stamp(&wal)?.is_some() {
        return Ok(None);
    }
    let index = VectorIndex::open_read_only(&path)
        .context("inspect completed semantic vectors without modifying them")?;
    if index.embedder_id() != engine.embedder_id()
        || Some(index.embedder_revision())
            != engine::expected_vector_space_revision(engine.embedder_id())
        || index.dimension() != artifact.dimension
        || index.quantization() != Quantization::F16
        || u64::try_from(index.record_count()).ok() != Some(artifact.doc_count)
        || index.wal_record_count() != 0
        || index.tombstone_count() != 0
    {
        return Ok(None);
    }
    // Borrow IDs from the retained read-only image instead of duplicating all
    // strings or retaining canonical message bodies. Visit vectors one at a time.
    let mut remaining = HashSet::with_capacity(index.record_count());
    for record in 0..index.record_count() {
        if !remaining.insert(index.doc_id_at(record)?) {
            return Ok(None);
        }
        let vector = index.vector_at_f32(record)?;
        let norm: f64 = vector.iter().map(|value| f64::from(*value).powi(2)).sum();
        if vector.iter().any(|value| !value.is_finite()) || !norm.is_finite() || norm == 0.0 {
            return Ok(None);
        }
    }
    sink.emit(
        SemanticProgressEvent::SelectionStart,
        SemanticProgressFields {
            rows_total: Some(artifact.doc_count),
            note: Some("cache miss: prove unchanged canonical semantic identities".into()),
            ..Default::default()
        },
    );
    let mut changed = false;
    let mut visited = 0u64;
    engine::visit_packet_embedding_inputs_from_storage(storage, |input| {
        if let Some(id) = engine::semantic_doc_id_for_input(&input) {
            visited = visited.saturating_add(1);
            // A duplicate canonical ID also fails: it was already removed.
            changed |= !remaining.remove(id.as_str());
            if visited.is_multiple_of(1024) {
                sink.emit(
                    SemanticProgressEvent::PacketReplayProgress,
                    SemanticProgressFields {
                        rows_processed: Some(visited),
                        rows_total: Some(artifact.doc_count),
                        ..Default::default()
                    },
                );
            }
        }
        Ok(())
    })?;
    // Conversation coverage is independent of passage cardinality. Do not
    // preserve stale coverage counters just because a message moved parents.
    let (conversations, last_offset): (i64, i64) = storage.raw().query_row_map(
        "SELECT COUNT(*), COALESCE(MAX(c.id), 0) FROM conversations c
         WHERE EXISTS (SELECT 1 FROM messages m WHERE m.conversation_id = c.id)",
        &[] as &[ParamValue],
        |row| Ok((row.get_typed(0)?, row.get_typed(1)?)),
    )?;
    checkpoint()?;
    // Detect mutation across the entire proof, including WAL-only commits,
    // restored mtimes, replaced pathnames, and a pinned old SQL snapshot.
    ensure!(
        ArchiveStamp::capture(storage)?.as_ref() == Some(&archive_before)
            && stamp(&path)? == vector_before
            && stamp(&wal)?.is_none()
            && stamp(&manifest_path)? == manifest_before
            && stamp(&pointer_path)?.is_none(),
        "archive or semantic artifacts changed during unchanged proof; reload and retry"
    );
    if changed
        || !remaining.is_empty()
        || u64::try_from(conversations).ok() != Some(artifact.conversation_count)
    {
        return Ok(None);
    }
    if let Err(error) = cache::refresh(
        data_dir,
        &archive_before,
        &path,
        vector_stamp,
        artifact,
        last_offset,
    ) {
        tracing::debug!(%error, "unchanged backfill cache unavailable; next pass repeats read-only proof");
    }
    sink.emit(
        SemanticProgressEvent::Complete,
        SemanticProgressFields {
            rows_processed: Some(artifact.conversation_count),
            rows_total: Some(artifact.conversation_count),
            note: Some("unchanged: exact canonical identities verified after cache miss; publication retained".into()),
            ..Default::default()
        },
    );
    Ok(Some(SemanticBackfillBatchOutcome {
        tier: plan.tier,
        embedder_id: artifact.embedder_id.clone(),
        embedded_docs: 0,
        conversations_processed: artifact.conversation_count,
        total_conversations: artifact.conversation_count,
        last_offset,
        checkpoint_saved: false,
        published: true,
        unchanged: true,
        index_path: path,
        manifest_path,
    }))
}

#[derive(Debug, PartialEq, Eq)]
struct FileStamp {
    regular: bool,
    identity: (u64, u64),
    len: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

fn stamp(path: &Path) -> Result<Option<FileStamp>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    };
    Ok(Some(FileStamp {
        regular: metadata.is_file(),
        identity: (metadata.dev(), metadata.ino()),
        len: metadata.len(),
        modified: (metadata.mtime(), metadata.mtime_nsec()),
        changed: (metadata.ctime(), metadata.ctime_nsec()),
    }))
}

#[derive(Debug, PartialEq, Eq)]
struct ArchiveStamp {
    path: PathBuf,
    main: FileStamp,
    wal: Option<FileStamp>,
}

impl ArchiveStamp {
    fn capture(storage: &FrankenStorage) -> Result<Option<Self>> {
        if storage.raw().as_async().in_transaction() {
            return Ok(None);
        }
        let path = storage.database_path()?.canonicalize()?;
        let Some(identity) = storage.raw().file_identity()? else {
            return Ok(None);
        };
        let file = File::open(&path)?;
        if crate::franken_sync::FileIdentity::from_file(&file)? != Some(identity) {
            return Ok(None);
        }
        let Some(main) = stamp(&path)? else {
            return Ok(None);
        };
        let wal = stamp(&crate::storage::sqlite::database_sidecar_path(
            &path, "-wal",
        ))?;
        if !main.regular || wal.as_ref().is_some_and(|wal| !wal.regular) {
            return Ok(None);
        }
        // Recheck against the live descriptor after both pathname observations.
        let current = File::open(&path)?;
        if crate::franken_sync::FileIdentity::from_file(&current)? != Some(identity) {
            return Ok(None);
        }
        Ok(Some(Self { path, main, wal }))
    }
}

#[cfg(test)]
mod tests;

mod cache;
