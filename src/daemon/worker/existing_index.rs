//! Retain a query-only generation while planning a daemon embedding pass.
//!
//! Coverage is the current main-plus-WAL view, not the physical main table.
//! Unusable or ambiguous rows need fresh embeddings; an open/lock error is not
//! a missing archive. Path observations supplement the caller's publication
//! serialization, rather than providing an atomic filesystem compare-and-swap.

use super::*;
use anyhow::{Context, ensure};
use frankensearch::index::wal_path_for;
use std::fs;
use std::path::PathBuf;

#[derive(Debug)]
struct Coverage {
    occurrences: usize,
    usable: bool,
}

#[derive(Debug)]
pub(super) struct ExistingIndexState {
    pub(super) path_exists: bool,
    source: Option<VectorIndex>,
    path: PathBuf,
    before: SourceFiles,
    compatible: bool,
    active: HashMap<String, Coverage>,
    tombstones: usize,
}

impl ExistingIndexState {
    /// None means cooperative cancellation, never a missing/corrupt generation.
    pub(super) fn open(
        index_path: &Path,
        kind: &WorkerEmbedderKind,
        cancelled: &dyn Fn() -> bool,
    ) -> anyhow::Result<Option<Self>> {
        if cancelled() {
            return Ok(None);
        }
        let (embedder_id, dimension) = match kind {
            WorkerEmbedderKind::Hash => {
                ("fnv1a-384", crate::search::hash_embedder::DEFAULT_DIMENSION)
            }
            WorkerEmbedderKind::FastEmbed {
                embedder_id,
                dimension,
                ..
            } => (embedder_id.as_str(), *dimension),
        };
        let path = vector_index_path(index_path, embedder_id);
        let before = SourceFiles::capture(&path)?;
        let source = if before.main.is_some() {
            Some(VectorIndex::open_read_only(&path).context("open daemon semantic reuse source")?)
        } else {
            ensure!(
                before.wal.is_none(),
                "daemon semantic source has an orphan WAL; recover its main generation first"
            );
            None
        };
        let mut state = Self {
            path_exists: source.is_some(),
            source,
            path,
            before,
            compatible: false,
            active: HashMap::new(),
            tombstones: 0,
        };
        state.ensure_current()?;
        if cancelled() {
            return Ok(None);
        }
        let Some(source) = state.source.as_ref() else {
            return Ok(Some(state));
        };
        state.compatible = expected_vector_space_revision(embedder_id).is_some_and(|revision| {
            source.embedder_id() == embedder_id
                && source.embedder_revision() == revision
                && source.dimension() == dimension
        });
        if state.compatible {
            let mut shadowed = HashSet::new();
            shadowed.try_reserve(source.wal_record_count())?;
            for (id, vector) in source.wal_records() {
                if cancelled() {
                    return Ok(None);
                }
                ensure!(shadowed.insert(id), "duplicate daemon semantic WAL identity");
                note_row(&mut state.active, id, usable(vector))?;
            }
            for row in 0..source.record_count() {
                if cancelled() {
                    return Ok(None);
                }
                if source.is_deleted(row) {
                    state.tombstones += 1;
                    continue;
                }
                let id = source.doc_id_at(row)?;
                // Even an unusable current WAL row supersedes the main row.
                // Re-embed it; never resurrect a usable but superseded value.
                if !shadowed.contains(id) {
                    note_row(&mut state.active, id, source.is_vector_usable(row))?;
                }
            }
        }
        state.ensure_current()?;
        if cancelled() {
            return Ok(None);
        }
        Ok(Some(state))
    }

    pub(super) fn active_count(&self, id: &str) -> usize {
        if !self.compatible {
            return 0;
        }
        self.active
            .get(id)
            .filter(|coverage| coverage.usable)
            .map_or(0, |coverage| coverage.occurrences)
    }

    pub(super) fn exactly_matches(&self, current: &HashSet<String>) -> bool {
        self.compatible
            && self.before.wal.is_none()
            && self.tombstones == 0
            && self
                .source
                .as_ref()
                .is_some_and(|source| source.record_count() == current.len())
            && self.active.len() == current.len()
            && current.iter().all(|id| self.active_count(id) == 1)
    }

    pub(super) fn ensure_current(&self) -> anyhow::Result<()> {
        ensure!(
            SourceFiles::capture(&self.path)? == self.before,
            "daemon semantic source changed during reuse planning; retry against the current generation"
        );
        Ok(())
    }
}

fn note_row(active: &mut HashMap<String, Coverage>, id: &str, usable: bool) -> anyhow::Result<()> {
    if let Some(coverage) = active.get_mut(id) {
        coverage.occurrences = coverage
            .occurrences
            .checked_add(1)
            .context("daemon coverage count overflow")?;
        coverage.usable &= usable;
    } else {
        active.try_reserve(1)?;
        active.insert(
            id.to_owned(),
            Coverage {
                occurrences: 1,
                usable,
            },
        );
    }
    Ok(())
}

fn usable(vector: &[f32]) -> bool {
    let norm = vector.iter().fold(0.0f32, |sum, value| sum + value * value);
    norm.is_finite() && norm > 0.0
}

#[derive(Debug, PartialEq, Eq)]
struct SourceFiles {
    main: Option<FileObservation>,
    wal: Option<FileObservation>,
}

impl SourceFiles {
    fn capture(path: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            main: FileObservation::capture(path)?,
            wal: FileObservation::capture(&wal_path_for(path))?,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
struct FileObservation {
    handle: same_file::Handle,
    len: u64,
    modified: std::time::SystemTime,
    #[cfg(unix)]
    changed: (i64, i64),
}

impl FileObservation {
    fn capture(path: &Path) -> anyhow::Result<Option<Self>> {
        let named = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("inspect daemon semantic source"),
        };
        ensure!(
            named.is_file(),
            "daemon semantic source must be a regular file, not a symlink or directory"
        );
        let handle = same_file::Handle::from_path(path)?;
        let metadata = handle.as_file().metadata()?;
        ensure!(
            metadata.is_file(),
            "opened daemon semantic source is not a regular file"
        );
        #[cfg(unix)]
        let changed = {
            use std::os::unix::fs::MetadataExt;
            ensure!(
                (named.dev(), named.ino()) == (metadata.dev(), metadata.ino()),
                "daemon semantic source changed while opening its observation"
            );
            (metadata.ctime(), metadata.ctime_nsec())
        };
        Ok(Some(Self {
            handle,
            len: metadata.len(),
            modified: metadata.modified()?,
            #[cfg(unix)]
            changed,
        }))
    }
}

#[cfg(test)]
mod tests;
