//! Approximate nearest-neighbor (ANN) reporting types.
//!
//! cass uses frankensearch's HNSW implementation for approximate semantic search.
//! This module intentionally stays small: it defines the stats payload surfaced
//! in robot output and TUI diagnostics.

use std::path::{Path, PathBuf};

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hnsw_index_path_uses_expected_layout() {
        let p = hnsw_index_path(Path::new("/tmp/cass"), "minilm-384");
        assert!(p.ends_with("vector_index/hnsw-minilm-384.chsw"));
    }
}
