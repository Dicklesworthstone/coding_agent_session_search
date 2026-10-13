//! Hold the CASS artifact lease through native graph verification/publication.
//! Native HNSW save locks serialize graph files, not their relationship to
//! backfill/rebuild/recovery. The caller still owns canonical source selection.

use super::*;

pub(super) fn run(
    indexer: &SemanticIndexer,
    vector_index: &VectorIndex,
    data_dir: &Path,
    m: Option<usize>,
    ef_construction: Option<usize>,
    mut check_cancelled: impl FnMut() -> Result<()>,
) -> Result<PathBuf> {
    check_cancelled()?;
    // Share the lock-only publication admission used by shards. In particular,
    // never sweep a caller's retained, unreferenced staging source before using
    // it to build a graph. Both ordinary success and failure only release the
    // lease; HNSW generation directories are not added to the recovery sweep.
    let _artifacts = super::super::artifacts::BackfillArtifacts::for_shard_publication(data_dir)?;
    check_cancelled()?;
    // Keep the native contract, configuration checks, existing-graph reuse and
    // readback in one implementation. Do not report cancellation after it has
    // acknowledged publication, or try to retire anything from the Drop path.
    indexer
        .inner
        .build_hnsw_index(vector_index, data_dir, m, ef_construction)
}
