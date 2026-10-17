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

mod main_coverage;
use main_coverage::MainCoverage;

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
    // The main file already owns its IDs. Only WAL overrides need a map.
    wal_coverage: HashMap<String, Coverage>,
    main: MainCoverage,
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
            wal_coverage: HashMap::new(),
            main: MainCoverage::default(),
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
            for (id, vector) in source.wal_records() {
                if cancelled() {
                    return Ok(None);
                }
                ensure!(
                    !state.wal_coverage.contains_key(id),
                    "duplicate daemon semantic WAL identity"
                );
                note_row(&mut state.wal_coverage, id, usable(vector))?;
            }
            let Some(main) = MainCoverage::inspect(source, cancelled)? else {
                return Ok(None);
            };
            state.main = main;
        }
        state.ensure_current()?;
        if cancelled() {
            return Ok(None);
        }
        Ok(Some(state))
    }

    pub(super) fn active_count(&self, id: &str) -> anyhow::Result<usize> {
        if !self.compatible {
            return Ok(0);
        }
        // An unusable current WAL value still shadows every main-file value.
        if let Some(coverage) = self.wal_coverage.get(id) {
            return Ok(if coverage.usable {
                coverage.occurrences
            } else {
                0
            });
        }
        let source = self
            .source
            .as_ref()
            .context("missing admitted semantic source")?;
        self.main.active_count(source, id)
    }

    pub(super) fn exactly_matches(&self, current: &HashSet<String>) -> anyhow::Result<bool> {
        if !self.compatible
            || self.before.wal.is_some()
            || self.main.tombstones() != 0
            || self
                .source
                .as_ref()
                .is_none_or(|source| source.record_count() != current.len())
        {
            return Ok(false);
        }
        // Equal physical count plus unique usable coverage for EVERY canonical
        // ID proves the bijection without copying all main-file identities.
        for id in current {
            if self.active_count(id)? != 1 {
                return Ok(false);
            }
        }
        Ok(true)
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
