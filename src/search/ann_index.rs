//! Approximate nearest-neighbor (ANN) reporting and accelerator maintenance.
//!
//! cass uses frankensearch's HNSW implementation for approximate semantic search.
//! Requested maintenance verifies the persisted native graph against its source
//! before preserving it or publishing a replacement. It never loads a model or
//! generates embeddings.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use frankensearch::index::{HnswConfig, HnswIndex, VectorIndex};

use crate::search::vector_index::VECTOR_INDEX_DIR;

/// Why native retrieval was replaced by the complete exact cohort.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AnnExactFallbackReason {
    /// Metadata filtering removed candidates before the message page filled.
    FilteredCandidateUnderfill,
    /// Multiple chunks or duplicate messages occupied the native windows.
    MessageCandidateUnderfill,
    /// A native shard query failed after owner and query validation. Native
    /// counters cover completed shard calls only; failed-call work is unknown.
    NativeSearchFailed,
    /// Durable updates extend or supersede the main slab covered by HNSW.
    /// A main-only native page cannot certify the complete retained snapshot.
    WalDeltaRequiresExact,
}

/// Work done by the complete exact cohort after native underfill or failure.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AnnExactFallbackStats {
    pub reason: AnnExactFallbackReason,
    pub shard_count: usize,
    /// Includes exact refills, not the preceding native search time.
    pub search_time_us: u64,
    /// Distinct candidates before hydration, noise filtering, and pagination.
    pub returned_messages: usize,
}

/// Statistics from an ANN search operation.
///
/// These metrics help users understand the quality/speed tradeoff of approximate search.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct AnnSearchStats {
    /// Total vectors in the HNSW index.
    pub index_size: usize,
    /// Dimension of vectors.
    pub dimension: usize,
    /// ef parameter used for this search (higher = more accurate but slower).
    pub ef_search: usize,
    /// Number of results requested (k).
    pub k_requested: usize,
    /// Number of results returned.
    pub k_returned: usize,
    /// Native backend search time in microseconds, including its own repairs.
    /// Additional CASS exact recovery is measured in `exact_fallback`.
    pub search_time_us: u64,
    /// Estimated recall based on ef/k ratio.
    ///
    /// Formula: min(1.0, 0.9 + 0.1 * log2(ef / k))
    /// This is an empirical estimate; actual recall depends on data distribution.
    /// Zero after native failure: an incomplete cohort has no recall estimate.
    pub estimated_recall: f32,
    /// Whether the returned candidate ranking remains approximate. False when
    /// the complete retained exact cohort replaced the native candidates.
    pub is_approximate: bool,
    /// Native counters above remain measurements of native work, not counts of
    /// exact results. This receipt identifies the separate recovery operation;
    /// `estimated_recall` remains the native heuristic, not an exact certificate.
    /// For `native_search_failed`, counters include only completed shard calls;
    /// they exclude the failed call and are not totals for the complete cohort.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exact_fallback: Option<AnnExactFallbackStats>,
}

/// Default on-disk location for the HNSW index for a given embedder.
#[must_use]
pub fn hnsw_index_path(data_dir: &Path, embedder_id: &str) -> PathBuf {
    data_dir
        .join(VECTOR_INDEX_DIR)
        .join(format!("hnsw-{embedder_id}.chsw"))
}

/// Admissibility of the persisted accelerator, using the runtime's native-only
/// loader. A graph rebuilt in memory is never evidence of a current sidecar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HnswAcceleratorState {
    Missing,
    InvalidEntry,
    NativeValid,
    StaleOrLegacy,
    Unreadable,
}

impl HnswAcceleratorState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::InvalidEntry => "invalid_entry",
            Self::NativeValid => "native_valid",
            Self::StaleOrLegacy => "stale_or_legacy",
            Self::Unreadable => "unreadable",
        }
    }

    pub(crate) fn is_current(self) -> bool {
        matches!(self, Self::NativeValid)
    }
}

/// Classify without writing, embedding, or rebuilding an in-memory graph.
pub(crate) fn inspect_hnsw_accelerator(
    ann_path: &Path,
    index: &VectorIndex,
) -> HnswAcceleratorState {
    load_hnsw_accelerator(ann_path, index).0
}

fn load_hnsw_accelerator(
    ann_path: &Path,
    index: &VectorIndex,
) -> (HnswAcceleratorState, Option<HnswIndex>) {
    match std::fs::symlink_metadata(ann_path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return (HnswAcceleratorState::Missing, None);
        }
        Err(_) => return (HnswAcceleratorState::Unreadable, None),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return (HnswAcceleratorState::InvalidEntry, None);
        }
        Ok(_) => {}
    }
    // A standalone accelerator is current only for a finalized source. Native
    // HNSW binds the main slab, not its WAL; query-time overlay support is a
    // separate serving path and cannot certify whole-source publication here.
    if index.wal_record_count() > 0 {
        return (HnswAcceleratorState::StaleOrLegacy, None);
    }
    match HnswIndex::try_load_native(ann_path, index) {
        Ok(Some(graph)) => (HnswAcceleratorState::NativeValid, Some(graph)),
        Ok(None) => (HnswAcceleratorState::StaleOrLegacy, None),
        Err(_) => (HnswAcceleratorState::Unreadable, None),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HnswMaintenance {
    pub(crate) before: HnswAcceleratorState,
    pub(crate) rebuilt: bool,
    pub(crate) reason: &'static str,
}

impl HnswMaintenance {
    pub(crate) fn action(self) -> &'static str {
        if self.rebuilt { "rebuilt" } else { "unchanged" }
    }
}

/// Keep a matching native graph or build and read back its replacement.
///
/// Callers serialize this mutation with their existing index/backfill lock and
/// publish HnswRecord only after success. Both operations still validate the
/// complete source and load the graph; unchanged is not an O(1) readiness probe.
/// Changed vectors currently require a full graph rebuild, not incremental ANN
/// insertion. Source FSVI bytes and the canonical archive remain untouched.
pub(crate) fn maintain_hnsw_accelerator(
    ann_path: &Path,
    index: &VectorIndex,
    config: HnswConfig,
    force: bool,
) -> Result<HnswMaintenance> {
    if index.wal_record_count() > 0 {
        bail!(
            "cannot publish a current HNSW accelerator while semantic vectors contain a pending WAL; complete 'cass index --semantic' before building HNSW"
        );
    }
    let (before, graph) = load_hnsw_accelerator(ann_path, index);
    if before == HnswAcceleratorState::InvalidEntry {
        bail!(
            "HNSW accelerator path {} is not a regular non-symlink file",
            ann_path.display()
        );
    }
    let reason = if force {
        "forced"
    } else if let Some(graph) = &graph {
        if graph.config() == config {
            tracing::info!(
                path = %ann_path.display(),
                vector_count = index.record_count(),
                action = "unchanged",
                reason = "native_current",
                "HNSW accelerator maintenance complete"
            );
            return Ok(HnswMaintenance {
                before,
                rebuilt: false,
                reason: "native_current",
            });
        }
        "config_changed"
    } else {
        before.as_str()
    };
    // Do not hold the prior full graph while constructing its replacement.
    drop(graph);

    if let Some(parent) = ann_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating HNSW directory {}", parent.display()))?;
    }
    let graph = HnswIndex::build_from_vector_index(index, config)
        .context("building HNSW accelerator from current vectors")?;
    graph
        .save(ann_path)
        .with_context(|| format!("saving HNSW accelerator {}", ann_path.display()))?;
    drop(graph);
    let after = inspect_hnsw_accelerator(ann_path, index);
    if !after.is_current() {
        bail!(
            "HNSW accelerator was written but does not load back as the native graph for its source (state: {}); the semantic manifest must remain unchanged",
            after.as_str()
        );
    }
    tracing::info!(
        path = %ann_path.display(),
        vector_count = index.record_count(),
        action = "rebuilt",
        reason,
        "HNSW accelerator maintenance complete"
    );
    Ok(HnswMaintenance {
        before,
        rebuilt: true,
        reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;
    use std::time::SystemTime;

    fn write_index(path: &Path, rows: &[(&str, [f32; 3])]) -> Result<VectorIndex> {
        let mut writer = VectorIndex::create_with_revision(
            path,
            "maintenance-test",
            "source-v1",
            3,
            frankensearch::index::Quantization::F16,
        )?;
        for (id, vector) in rows {
            writer.write_record(id, vector)?;
        }
        writer.finish()?;
        Ok(VectorIndex::open_read_only(path)?)
    }

    fn file_snapshot(root: &Path) -> Result<BTreeMap<PathBuf, (Vec<u8>, SystemTime)>> {
        let mut snapshot = BTreeMap::new();
        for entry in walkdir::WalkDir::new(root) {
            let entry = entry?;
            if entry.file_type().is_file() {
                snapshot.insert(
                    entry.path().strip_prefix(root)?.to_path_buf(),
                    (fs::read(entry.path())?, entry.metadata()?.modified()?),
                );
            }
        }
        Ok(snapshot)
    }

    #[test]
    fn hnsw_index_path_uses_expected_layout() {
        let p = hnsw_index_path(Path::new("/tmp/cass"), "minilm-384");
        assert!(p.ends_with("vector_index/hnsw-minilm-384.chsw"));
    }

    #[test]
    fn maintenance_preserves_current_native_files_and_honors_configuration() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source = write_index(
            &temp.path().join("source.fsvi"),
            &[("first", [1.0, 0.0, 0.0]), ("second", [0.0, 1.0, 0.0])],
        )?;
        let ann = temp.path().join("accelerator.chsw");
        let config = HnswConfig::default();
        let built = maintain_hnsw_accelerator(&ann, &source, config, false)?;
        assert_eq!(built.before, HnswAcceleratorState::Missing);
        assert!(built.rebuilt);
        assert_eq!(built.reason, "missing");
        let before = file_snapshot(temp.path())?;
        let unchanged = maintain_hnsw_accelerator(&ann, &source, config, false)?;
        assert_eq!(unchanged.before, HnswAcceleratorState::NativeValid);
        assert!(!unchanged.rebuilt);
        assert_eq!(unchanged.reason, "native_current");
        assert_eq!(file_snapshot(temp.path())?, before);

        let changed_config = HnswConfig { m: 8, ..config };
        let changed = maintain_hnsw_accelerator(&ann, &source, changed_config, false)?;
        assert!(changed.rebuilt);
        assert_eq!(changed.reason, "config_changed");
        let native = HnswIndex::try_load_native(&ann, &source)?.context("native graph")?;
        assert_eq!(native.config(), changed_config);
        drop(native);
        assert!(!maintain_hnsw_accelerator(&ann, &source, changed_config, false)?.rebuilt);
        let forced = maintain_hnsw_accelerator(&ann, &source, changed_config, true)?;
        assert!(forced.rebuilt);
        assert_eq!(forced.reason, "forced");
        Ok(())
    }

    #[test]
    fn maintenance_repairs_native_corruption_without_changing_vectors() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source_path = temp.path().join("source.fsvi");
        let source = write_index(
            &source_path,
            &[("first", [1.0, 0.0, 0.0]), ("second", [0.0, 1.0, 0.0])],
        )?;
        let source_bytes = fs::read(&source_path)?;
        let ann = temp.path().join("accelerator.chsw");
        let config = HnswConfig::default();
        maintain_hnsw_accelerator(&ann, &source, config, false)?;
        let metadata: serde_json::Value = serde_json::from_slice(&fs::read(&ann)?)?;
        let generation = metadata["sidecar_generation"]
            .as_str()
            .context("generation")?;
        let basename = metadata["sidecar_basename"].as_str().context("basename")?;
        let graph_path = temp
            .path()
            .join(generation)
            .join(format!("{basename}.hnsw.graph"));
        fs::write(&graph_path, b"truncated native graph")?;
        assert_eq!(
            inspect_hnsw_accelerator(&ann, &source),
            HnswAcceleratorState::StaleOrLegacy
        );
        let repaired = maintain_hnsw_accelerator(&ann, &source, config, false)?;
        assert!(repaired.rebuilt);
        assert_eq!(repaired.reason, "stale_or_legacy");
        assert_eq!(
            inspect_hnsw_accelerator(&ann, &source),
            HnswAcceleratorState::NativeValid
        );
        assert_eq!(fs::read(&source_path)?, source_bytes);
        Ok(())
    }

    #[test]
    fn maintenance_replaces_stale_graph_after_vector_edits_and_appends() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let prior = write_index(
            &temp.path().join("prior.fsvi"),
            &[("first", [1.0, 0.0, 0.0]), ("second", [0.0, 0.0, 1.0])],
        )?;
        let ann = temp.path().join("accelerator.chsw");
        let config = HnswConfig::default();
        maintain_hnsw_accelerator(&ann, &prior, config, false)?;
        // Same identifiers/count/dimension, different coordinates. Filename or
        // row-count matching alone would incorrectly keep the old graph.
        let edited = write_index(
            &temp.path().join("edited.fsvi"),
            &[("first", [0.0, 1.0, 0.0]), ("second", [0.0, 0.0, 1.0])],
        )?;
        assert!(HnswIndex::try_load_native(&ann, &edited)?.is_none());
        assert!(maintain_hnsw_accelerator(&ann, &edited, config, false)?.rebuilt);
        let native = HnswIndex::try_load_native(&ann, &edited)?.context("edited native graph")?;
        let hits = native.knn_search_against(&edited, &[0.0, 1.0, 0.0], 1, 20)?;
        assert_eq!(hits[0].doc_id.as_str(), "first");
        drop(native);

        let appended = write_index(
            &temp.path().join("appended.fsvi"),
            &[
                ("first", [0.0, 1.0, 0.0]),
                ("second", [0.0, 0.0, 1.0]),
                ("new", [1.0, 0.0, 0.0]),
            ],
        )?;
        assert!(maintain_hnsw_accelerator(&ann, &appended, config, false)?.rebuilt);
        let native =
            HnswIndex::try_load_native(&ann, &appended)?.context("appended native graph")?;
        let hits = native.knn_search_against(&appended, &[1.0, 0.0, 0.0], 1, 20)?;
        assert_eq!(hits[0].doc_id.as_str(), "new");
        assert!(native.matches_vector_index(&appended)?);
        assert!(!native.matches_vector_index(&prior)?);
        Ok(())
    }

    #[test]
    fn maintenance_refuses_pending_wal_without_mutating_source_or_graph() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source_path = temp.path().join("source.fsvi");
        let source = write_index(&source_path, &[("first", [1.0, 0.0, 0.0])])?;
        let ann = temp.path().join("accelerator.chsw");
        let config = HnswConfig::default();
        maintain_hnsw_accelerator(&ann, &source, config, false)?;
        drop(source);
        let mut writer = VectorIndex::open_writer(&source_path)?;
        writer.append_batch(&[("new".to_owned(), vec![0.0, 1.0, 0.0])])?;
        drop(writer);
        let source = VectorIndex::open_read_only(&source_path)?;
        assert_eq!(source.wal_record_count(), 1);
        // Upstream graph admission concerns only the main slab. The CASS
        // publication check must also notice the unfinished durable delta.
        assert!(HnswIndex::try_load_native(&ann, &source)?.is_some());
        assert_eq!(
            inspect_hnsw_accelerator(&ann, &source),
            HnswAcceleratorState::StaleOrLegacy
        );
        let before = file_snapshot(temp.path())?;
        let error = maintain_hnsw_accelerator(&ann, &source, config, false)
            .expect_err("unfinalized WAL cannot be advertised as a current whole-source graph");
        assert!(error.to_string().contains("pending WAL"));
        assert_eq!(file_snapshot(temp.path())?, before);
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn maintenance_refuses_a_symlink_without_mutating_its_target() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source = write_index(
            &temp.path().join("source.fsvi"),
            &[("first", [1.0, 0.0, 0.0])],
        )?;
        let target = temp.path().join("external-metadata");
        fs::write(&target, b"unrelated retained data")?;
        let ann = temp.path().join("accelerator.chsw");
        std::os::unix::fs::symlink(&target, &ann)?;
        assert_eq!(
            inspect_hnsw_accelerator(&ann, &source),
            HnswAcceleratorState::InvalidEntry
        );
        assert!(maintain_hnsw_accelerator(&ann, &source, HnswConfig::default(), false).is_err());
        assert_eq!(fs::read(&target)?, b"unrelated retained data");
        assert!(fs::symlink_metadata(&ann)?.file_type().is_symlink());
        Ok(())
    }
}
