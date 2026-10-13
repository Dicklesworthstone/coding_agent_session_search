//! A saved ready record is not sufficient authority to retire fallback data.
//! Read through the native, non-repairing reader, then make its main/WAL pair
//! durable. Not-ready records, ANN and selected-generation references retain
//! their ownership independently of ready legacy and shard data admission.

use super::*;
use crate::search::semantic_manifest::ArtifactRecord;
use frankensearch::index::VectorIndex;

mod vectors;

/// Data-admission failure, not metadata/cursor authority. Startup may defer
/// cleanup on this exact type while allowing an explicit canonical backfill
/// to repair coverage. Preview/apply still fail with all candidates retained.
#[derive(Debug, thiserror::Error)]
#[error("published semantic index {path} cannot authorize reclamation; retaining fallback artifacts", path = .path.display())]
pub(super) struct PublicationUnavailable {
    path: PathBuf,
}

pub(super) fn validate_ready(
    data_dir: &Path,
    manifest: &SemanticManifest,
    durable: bool,
) -> Result<()> {
    validate_with(data_dir, manifest, |path, main, wal_path, wal| {
        pin_data(path, main, wal_path, wal, durable)
    })
}

/// Use the exact sidecar snapshot that supplied discovery's protected paths.
/// A shard-only archive need not have a legacy SemanticManifest at all. Ready
/// shards in partial or older groups still acknowledge data: do not discard
/// their fallbacks merely because the group is not currently queryable.
pub(super) fn validate_shards(
    data_dir: &Path,
    manifest: &SemanticShardManifest,
    durable: bool,
) -> Result<()> {
    validate_shards_with(data_dir, manifest, |path, main, wal_path, wal| {
        pin_data(path, main, wal_path, wal, durable)
    })
}

fn pin_data(
    path: &Path,
    main: &File,
    wal_path: &Path,
    wal: Option<&File>,
    durable: bool,
) -> Result<()> {
    if durable {
        sync_file(path, main)?;
        if let Some(wal) = wal {
            sync_file(wal_path, wal)?;
            sync_reference_parent(wal_path)?;
        }
        sync_reference_parent(path)?;
    }
    Ok(())
}

fn validate_with(
    data_dir: &Path,
    manifest: &SemanticManifest,
    mut pin: impl FnMut(&Path, &File, &Path, Option<&File>) -> Result<()>,
) -> Result<()> {
    for artifact in [&manifest.fast_tier, &manifest.quality_tier]
        .into_iter()
        .flatten()
        .filter(|artifact| artifact.ready)
    {
        let path = data_dir.join(&artifact.index_path);
        validate_one(&path, artifact, &mut pin)
            .with_context(|| PublicationUnavailable { path })?;
    }
    Ok(())
}

fn validate_shards_with(
    data_dir: &Path,
    manifest: &SemanticShardManifest,
    mut pin: impl FnMut(&Path, &File, &Path, Option<&File>) -> Result<()>,
) -> Result<()> {
    for shard in manifest.shards.iter().filter(|shard| shard.ready) {
        let path = data_dir.join(&shard.index_path);
        validate_data(
            &path,
            &shard.embedder_id,
            shard.dimension,
            shard.doc_count,
            &mut pin,
        )
        .with_context(|| PublicationUnavailable { path })?;
    }
    Ok(())
}

fn validate_one(
    path: &Path,
    artifact: &ArtifactRecord,
    pin: &mut impl FnMut(&Path, &File, &Path, Option<&File>) -> Result<()>,
) -> Result<()> {
    validate_data(
        path,
        &artifact.embedder_id,
        artifact.dimension,
        artifact.doc_count,
        pin,
    )
}

fn validate_data(
    path: &Path,
    embedder_id: &str,
    dimension: usize,
    doc_count: u64,
    pin: &mut impl FnMut(&Path, &File, &Path, Option<&File>) -> Result<()>,
) -> Result<()> {
    // Check regular-file types before opening, including the WAL, so a FIFO
    // cannot hang recovery. Retain these descriptors until after data sync.
    let main = open_checkpoint_file(path)?.context("published vector file is missing")?;
    let wal_path = wal_path_for(path);
    let wal = open_checkpoint_file(&wal_path)?;
    let index = VectorIndex::open_read_only(path)
        .context("open published vectors without repairing them")?;
    ensure!(
        index.embedder_id() == embedder_id && index.dimension() == dimension,
        "published vector producer or dimension differs from its ready record"
    );
    let available = index
        .record_count()
        .checked_sub(index.tombstone_count())
        .and_then(|main| main.checked_add(index.wal_record_count()))
        .context("published vector record count is invalid")?;
    ensure!(
        u64::try_from(available)? >= doc_count,
        "published vector image has fewer live records than its ready record"
    );
    // Updates can leave a main/WAL overlap, while ordinary appends can extend
    // an older ready record. Require at least the recorded distinct count,
    // not exact physical count, size, generation, or a model revision guess.
    checkpoint_coverage::require_distinct_live(&index, doc_count)?;
    // A valid header and sufficient IDs do not prove usable persisted signal.
    // Admit the effective payload before data sync can authorize reclamation.
    vectors::require_usable_live_vectors(&index)?;
    pin(path, &main, &wal_path, wal.as_ref())
}

fn sync_reference_parent(path: &Path) -> Result<()> {
    let parent = path.parent().context("published vector has no directory")?;
    sync_directory(parent)?;
    // A manifest may intentionally alias a staging-looking file. Keep both
    // names protected and sync the real file's directory as well as the alias.
    let resolved = path.canonicalize()?;
    let real_parent = resolved.parent().context("resolved vector has no directory")?;
    if real_parent != parent {
        sync_directory(real_parent)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_shards;
