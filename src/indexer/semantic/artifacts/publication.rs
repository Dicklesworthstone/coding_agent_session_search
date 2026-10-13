//! A saved ready record is not sufficient authority to retire fallback data.
//! Read through the native, non-repairing reader, then make its main/WAL pair
//! durable. Ownership of not-ready tiers, ANN, shards and selections remains
//! independent of this additional admission of ready legacy publications.

use super::*;
use crate::search::semantic_manifest::ArtifactRecord;
use frankensearch::index::VectorIndex;

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
        if durable {
            sync_file(path, main)?;
            if let Some(wal) = wal {
                sync_file(wal_path, wal)?;
                sync_reference_parent(wal_path)?;
            }
            sync_reference_parent(path)?;
        }
        Ok(())
    })
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

fn validate_one(
    path: &Path,
    artifact: &ArtifactRecord,
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
        index.embedder_id() == artifact.embedder_id && index.dimension() == artifact.dimension,
        "published vector producer or dimension differs from its ready record"
    );
    let available = index
        .record_count()
        .checked_sub(index.tombstone_count())
        .and_then(|main| main.checked_add(index.wal_record_count()))
        .context("published vector record count is invalid")?;
    ensure!(
        u64::try_from(available)? >= artifact.doc_count,
        "published vector image has fewer live records than its ready record"
    );
    // Updates can leave a main/WAL overlap, while ordinary appends can extend
    // an older ready record. Require at least the recorded distinct count,
    // not exact physical count, size, generation, or a model revision guess.
    checkpoint_coverage::require_distinct_live(&index, artifact.doc_count)?;
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
