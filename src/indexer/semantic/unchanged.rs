//! A completed-build cache is a skip hint, not permission to rewrite serving
//! assets. On a cache miss, prove a zero canonical delta before entering the
//! engine's mutating reconciliation. No model is run and no cache is trusted
//! as canonical-content evidence here.
//!
//! This is limited to ready legacy-manifest artifacts on Unix, including their
//! certified immutable owners, where descriptor identity and ctime can fence a
//! read-only proof. Other formats/platforms keep the existing reconciliation
//! path. Existing ANN/readiness records are never promoted or repaired here.

use super::engine::{self, SemanticBackfillBatchOutcome, SemanticBackfillStoragePlan};
use crate::indexer::semantic_progress::{
    SemanticProgressEvent, SemanticProgressFields, SemanticProgressSink,
};
use crate::search::policy::{CHUNKING_STRATEGY_VERSION, SEMANTIC_SCHEMA_VERSION};
use crate::search::semantic_coverage::{
    SEMANTIC_PARENT_HEADER_BYTES, SEMANTIC_PARENT_MAGIC, SEMANTIC_PARENT_RECORD_BYTES,
    open_semantic_proof_file,
};
use crate::search::semantic_manifest::{
    SemanticCoverageCertificate, SemanticCurrentPointerV1, SemanticManifest, TierKind,
};
use crate::search::vector_index::vector_index_path;
use crate::storage::sqlite::FrankenStorage;
use anyhow::{Context, Result, ensure};
use frankensearch::index::{Quantization, VectorIndex, wal_path_for};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read};
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
    engine.check_external_cancelled()?;
    if !crate::search::external_embedder::is_external_identity(engine.embedder_id())
        && storage.semantic_archive_state()?.is_some()
        && !manifest.coverage.contains_key(engine.embedder_id())
    {
        // Preserve vectors, but perform one canonical reconciliation before
        // granting retained-generation serving to an older installation.
        return Ok(None);
    }
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
    engine.check_external_cancelled()?;
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
    let certificate = manifest.coverage.get(engine.embedder_id());
    let recorded_path = data_dir.join(&artifact.index_path);
    let path = if let Some(certificate) = certificate {
        if !certificate.matches_artifact(artifact)
            || Some(certificate.vector_space_revision.as_str())
                != engine::expected_vector_space_revision(engine.embedder_id())
        {
            return Ok(None);
        }
        recorded_path
    } else {
        let conventional_path = vector_index_path(data_dir, engine.embedder_id());
        if recorded_path != conventional_path {
            return Ok(None);
        }
        conventional_path
    };
    if !artifact.ready
        || artifact.tier != plan.tier
        || artifact.embedder_id != engine.embedder_id()
        || artifact.dimension != engine.embedder_dimension()
        || artifact.model_revision != plan.model_revision
        || artifact.schema_version != SEMANTIC_SCHEMA_VERSION
        || artifact.chunking_version != CHUNKING_STRATEGY_VERSION
        || artifact.db_fingerprint != plan.db_fingerprint
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
    let certified_files = if let Some(certificate) = certificate {
        let Some(files) = RetainedCoverageFiles::open(engine, data_dir, certificate, vector_stamp)?
        else {
            return Ok(None);
        };
        Some(files)
    } else {
        None
    };
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
    let mut remaining = HashSet::new();
    remaining.try_reserve(index.record_count())?;
    for record in 0..index.record_count() {
        engine.check_external_cancelled()?;
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
    let (comparison, coverage) =
        scan::compare_with_coverage(storage, &mut remaining, sink, artifact.doc_count, || {
            engine.check_external_cancelled()
        })?;
    tracing::debug!(
        visited = comparison.visited,
        matches = comparison.matches,
        "compared canonical replay with retained semantic identities"
    );
    // Vector identities omit some canonical parent provenance. A copied or
    // reparented message must not renew the old certificate just because its
    // text and vector ID happen to be unchanged.
    let parents_match = comparison.matches
        && match &certified_files {
            Some(files) => files.matches_canonical(engine, storage)?,
            None => true,
        };
    engine.check_external_cancelled()?;
    // Replay already observed every nonempty canonical parent. Reuse its exact
    // count and maximum rather than issuing separate count/existence probes.
    // Partial or mismatching replay supplies no coverage. Keep all final file
    // and cancellation fences before accepting this receipt or a cache miss.
    checkpoint()?;
    engine.check_external_cancelled()?;
    // Detect mutation across the entire proof, including WAL-only commits,
    // restored mtimes, replaced pathnames, and a pinned old SQL snapshot.
    ensure!(
        ArchiveStamp::capture(storage)?.as_ref() == Some(&archive_before)
            && stamp(&path)? == vector_before
            && stamp(&wal)?.is_none()
            && stamp(&manifest_path)? == manifest_before
            && stamp(&pointer_path)?.is_none()
            && match &certified_files {
                Some(files) => files.unchanged(data_dir, vector_stamp)?,
                None => true,
            },
        "archive or semantic artifacts changed during unchanged proof; reload and retry"
    );
    let Some((conversations, last_offset)) = coverage else {
        return Ok(None);
    };
    if conversations != artifact.conversation_count || !parents_match {
        return Ok(None);
    }
    // A cancelled proof cannot renew its skip receipt or report completion.
    engine.check_external_cancelled()?;
    if let Err(error) = cache::refresh(
        data_dir,
        &archive_before,
        &path,
        vector_stamp,
        artifact,
        last_offset,
        certified_files
            .as_ref()
            .map(|files| (files.certificate, &files.parents_before)),
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

/// Retain the exact certified owners while proving a cache miss. Neither the
/// mutable builder nor a pathname-only parent receipt can stand in for them.
struct RetainedCoverageFiles<'a> {
    certificate: &'a SemanticCoverageCertificate,
    vectors: File,
    parents: File,
    parents_before: FileStamp,
}

impl<'a> RetainedCoverageFiles<'a> {
    fn open(
        engine: &engine::SemanticIndexer,
        data_dir: &Path,
        certificate: &'a SemanticCoverageCertificate,
        vectors_before: &FileStamp,
    ) -> Result<Option<Self>> {
        let mut vectors = open_semantic_proof_file(data_dir, &certificate.artifact_path)?;
        if FileStamp::from_metadata(&vectors.metadata()?) != *vectors_before
            || file_digest_with_cancellation(engine, &mut vectors)? != certificate.artifact_sha256
        {
            return Ok(None);
        }
        let mut parents = open_semantic_proof_file(data_dir, &certificate.parent_path)?;
        let parents_before = FileStamp::from_metadata(&parents.metadata()?);
        let parent_bytes = certificate
            .parent_count
            .checked_mul(SEMANTIC_PARENT_RECORD_BYTES)
            .and_then(|bytes| bytes.checked_add(SEMANTIC_PARENT_HEADER_BYTES))
            .context("semantic parent file length overflow")?;
        if parents_before.len != parent_bytes
            || file_digest_with_cancellation(engine, &mut parents)? != certificate.parent_sha256
        {
            return Ok(None);
        }
        Ok(Some(Self {
            certificate,
            vectors,
            parents,
            parents_before,
        }))
    }

    fn matches_canonical(
        &self,
        engine: &engine::SemanticIndexer,
        storage: &FrankenStorage,
    ) -> Result<bool> {
        use crate::franken_sync::compat::TransactionExt;

        engine.check_external_cancelled()?;
        let mut transaction = storage.raw().transaction()?;
        let certificate = self.certificate;
        if !storage.semantic_archive_state()?.is_some_and(|state| {
            state.archive_id == certificate.archive_id
                && state.mutation_seq == certificate.mutation_seq
        }) || storage.max_message_id()?.unwrap_or(0) != certificate.max_message_id
        {
            transaction.rollback()?;
            return Ok(false);
        }
        // Reproduce the existing parent-file framing in constant auxiliary
        // memory. Its digest includes provenance that the vector IDs omit.
        let mut digest = Sha256::new();
        digest.update(SEMANTIC_PARENT_MAGIC);
        digest.update(certificate.parent_count.to_le_bytes());
        let mut count = 0_u64;
        crate::storage::sqlite::visit_semantic_parent_identities(storage.raw(), |id, identity| {
            engine.check_external_cancelled()?;
            count = count
                .checked_add(1)
                .context("semantic parent count overflow")?;
            digest.update(id.to_le_bytes());
            digest.update(identity);
            Ok(())
        })?;
        transaction.rollback()?;
        engine.check_external_cancelled()?;
        Ok(count == certificate.parent_count
            && hex::encode(digest.finalize()) == certificate.parent_sha256)
    }

    fn unchanged(&self, data_dir: &Path, vectors_before: &FileStamp) -> Result<bool> {
        // Reopen with the same contained, non-symlink path rules as admission,
        // and compare both pathnames and the descriptors retained above.
        let current_vectors = open_semantic_proof_file(data_dir, &self.certificate.artifact_path)?;
        let current_parents = open_semantic_proof_file(data_dir, &self.certificate.parent_path)?;
        Ok(
            FileStamp::from_metadata(&self.vectors.metadata()?) == *vectors_before
                && FileStamp::from_metadata(&current_vectors.metadata()?) == *vectors_before
                && FileStamp::from_metadata(&self.parents.metadata()?) == self.parents_before
                && FileStamp::from_metadata(&current_parents.metadata()?) == self.parents_before,
        )
    }
}

fn file_digest_with_cancellation(
    engine: &engine::SemanticIndexer,
    file: &mut File,
) -> Result<String> {
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        engine.check_external_cancelled()?;
        let count = file.read(&mut buffer)?;
        if count == 0 {
            return Ok(hex::encode(digest.finalize()));
        }
        digest.update(&buffer[..count]);
    }
}

#[derive(Debug, PartialEq, Eq)]
struct FileStamp {
    regular: bool,
    identity: (u64, u64),
    len: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl FileStamp {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            regular: metadata.is_file(),
            identity: (metadata.dev(), metadata.ino()),
            len: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }
}

fn stamp(path: &Path) -> Result<Option<FileStamp>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    };
    Ok(Some(FileStamp::from_metadata(&metadata)))
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

#[cfg(test)]
mod cancellation_tests;

mod cache;
mod scan;
