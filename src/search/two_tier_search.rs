//! Two-tier progressive search for session search (bd-3dcw).
//!
//! This module implements a progressive search strategy that:
//! 1. Returns instant results using a fast embedding model (in-process)
//! 2. Refines rankings in the background using a quality model (daemon)
//!
//! # Architecture
//!
//! ```text
//! User Query
//!     │
//!     ├──→ [Fast Embedder] ──→ Results in ~1ms (display immediately)
//!     │       (in-process)
//!     │
//!     └──→ [Quality Daemon] ──→ Refined scores in ~130ms
//!              (warm UDS)           │
//!                                   ▼
//!                           Smooth re-rank
//! ```
//!
//! # Usage
//!
//! ```ignore
//! use cass::search::two_tier_search::{TwoTierIndex, TwoTierConfig, SearchPhase};
//!
//! let index = TwoTierIndex::load(&data_dir)?;
//! let mut searcher = TwoTierSearcher::new(&index, daemon_client);
//!
//! for phase in searcher.search("authentication middleware", 10) {
//!     match phase {
//!         SearchPhase::Initial { results, latency_ms } => {
//!             // Display instant results
//!         }
//!         SearchPhase::Refined { results, latency_ms } => {
//!             // Update with refined results
//!         }
//!         SearchPhase::RefinementFailed { error } => {
//!             // Keep showing initial results
//!         }
//!     }
//! }
//! ```

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Result, bail};
use half::f16;
use tracing::{debug, warn};

use super::daemon_client::{DaemonClient, DaemonError};
use super::embedder::Embedder;

/// Configuration for two-tier search.
#[derive(Debug, Clone)]
pub struct TwoTierConfig {
    /// Dimension for fast embeddings (default: 256).
    pub fast_dimension: usize,
    /// Dimension for quality embeddings (default: 384).
    pub quality_dimension: usize,
    /// Weight for quality scores when blending (default: 0.7).
    pub quality_weight: f32,
    /// Maximum documents to refine via daemon (default: 100).
    pub max_refinement_docs: usize,
    /// Whether to skip quality refinement entirely.
    pub fast_only: bool,
    /// Whether to wait for quality results before returning.
    pub quality_only: bool,
}

impl Default for TwoTierConfig {
    fn default() -> Self {
        Self {
            fast_dimension: 256,
            quality_dimension: 384,
            quality_weight: 0.7,
            max_refinement_docs: 100,
            fast_only: false,
            quality_only: false,
        }
    }
}

impl TwoTierConfig {
    /// Load config from environment variables.
    pub fn from_env() -> Self {
        let mut cfg = Self::default();

        if let Ok(val) = dotenvy::var("CASS_TWO_TIER_FAST_DIM")
            && let Ok(dim) = val.parse()
        {
            cfg.fast_dimension = dim;
        }

        if let Ok(val) = dotenvy::var("CASS_TWO_TIER_QUALITY_DIM")
            && let Ok(dim) = val.parse()
        {
            cfg.quality_dimension = dim;
        }

        if let Ok(val) = dotenvy::var("CASS_TWO_TIER_QUALITY_WEIGHT")
            && let Ok(weight) = val.parse()
        {
            cfg.quality_weight = weight;
        }

        if let Ok(val) = dotenvy::var("CASS_TWO_TIER_MAX_REFINEMENT")
            && let Ok(max) = val.parse()
        {
            cfg.max_refinement_docs = max;
        }

        cfg
    }

    /// Create config for fast-only mode.
    pub fn fast_only() -> Self {
        Self {
            fast_only: true,
            ..Self::default()
        }
    }

    /// Create config for quality-only mode.
    pub fn quality_only() -> Self {
        Self {
            quality_only: true,
            ..Self::default()
        }
    }
}

/// Document identifier for two-tier index entries.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DocumentId {
    /// Full session document.
    Session(String),
    /// Session turn (session_id, turn_index).
    Turn(String, usize),
    /// Code block within a turn (session_id, turn_index, code_block_index).
    CodeBlock(String, usize, usize),
}

impl DocumentId {
    /// Get the session ID.
    pub fn session_id(&self) -> &str {
        match self {
            Self::Session(id) => id,
            Self::Turn(id, _) => id,
            Self::CodeBlock(id, _, _) => id,
        }
    }
}

/// Metadata for a two-tier index.
#[derive(Debug, Clone)]
pub struct TwoTierMetadata {
    /// Fast embedder ID (e.g., "potion-128m").
    pub fast_embedder_id: String,
    /// Quality embedder ID (e.g., "minilm-384").
    pub quality_embedder_id: String,
    /// Document count.
    pub doc_count: usize,
    /// Index build timestamp (Unix seconds).
    pub built_at: i64,
    /// Index status.
    pub status: IndexStatus,
}

/// Index build status.
#[derive(Debug, Clone)]
pub enum IndexStatus {
    /// Index is being built.
    Building { progress: f32 },
    /// Index is complete.
    Complete {
        fast_latency_ms: u64,
        quality_latency_ms: u64,
    },
    /// Index build failed.
    Failed { error: String },
}

/// Two-tier index entry with both fast and quality embeddings.
#[derive(Debug, Clone)]
pub struct TwoTierEntry {
    /// Document identifier.
    pub doc_id: DocumentId,
    /// Message ID for SQLite lookup.
    pub message_id: u64,
    /// Fast embedding (f16 quantized).
    pub fast_embedding: Vec<f16>,
    /// Quality embedding (f16 quantized).
    pub quality_embedding: Vec<f16>,
}

/// Two-tier index for progressive search.
#[derive(Debug)]
pub struct TwoTierIndex {
    /// Index metadata.
    pub metadata: TwoTierMetadata,
    /// Fast embeddings (row-major, f16).
    fast_embeddings: Vec<f16>,
    /// Quality embeddings (row-major, f16).
    quality_embeddings: Vec<f16>,
    /// Document IDs in index order.
    doc_ids: Vec<DocumentId>,
    /// Message IDs for SQLite lookup.
    message_ids: Vec<u64>,
}

impl TwoTierIndex {
    /// Build a two-tier index from entries.
    pub fn build(
        fast_embedder_id: impl Into<String>,
        quality_embedder_id: impl Into<String>,
        config: &TwoTierConfig,
        entries: impl IntoIterator<Item = TwoTierEntry>,
    ) -> Result<Self> {
        let entries: Vec<TwoTierEntry> = entries.into_iter().collect();
        let doc_count = entries.len();

        if doc_count == 0 {
            return Ok(Self {
                metadata: TwoTierMetadata {
                    fast_embedder_id: fast_embedder_id.into(),
                    quality_embedder_id: quality_embedder_id.into(),
                    doc_count: 0,
                    built_at: chrono::Utc::now().timestamp(),
                    status: IndexStatus::Complete {
                        fast_latency_ms: 0,
                        quality_latency_ms: 0,
                    },
                },
                fast_embeddings: Vec::new(),
                quality_embeddings: Vec::new(),
                doc_ids: Vec::new(),
                message_ids: Vec::new(),
            });
        }

        // Validate dimensions
        for (i, entry) in entries.iter().enumerate() {
            if entry.fast_embedding.len() != config.fast_dimension {
                bail!(
                    "fast embedding dimension mismatch at index {}: expected {}, got {}",
                    i,
                    config.fast_dimension,
                    entry.fast_embedding.len()
                );
            }
            if entry.quality_embedding.len() != config.quality_dimension {
                bail!(
                    "quality embedding dimension mismatch at index {}: expected {}, got {}",
                    i,
                    config.quality_dimension,
                    entry.quality_embedding.len()
                );
            }
        }

        // Build flat vectors
        let mut fast_embeddings = Vec::with_capacity(doc_count * config.fast_dimension);
        let mut quality_embeddings = Vec::with_capacity(doc_count * config.quality_dimension);
        let mut doc_ids = Vec::with_capacity(doc_count);
        let mut message_ids = Vec::with_capacity(doc_count);

        for entry in entries {
            fast_embeddings.extend(entry.fast_embedding);
            quality_embeddings.extend(entry.quality_embedding);
            doc_ids.push(entry.doc_id);
            message_ids.push(entry.message_id);
        }

        Ok(Self {
            metadata: TwoTierMetadata {
                fast_embedder_id: fast_embedder_id.into(),
                quality_embedder_id: quality_embedder_id.into(),
                doc_count,
                built_at: chrono::Utc::now().timestamp(),
                status: IndexStatus::Complete {
                    fast_latency_ms: 0,
                    quality_latency_ms: 0,
                },
            },
            fast_embeddings,
            quality_embeddings,
            doc_ids,
            message_ids,
        })
    }

    /// Get the number of documents in the index.
    pub fn len(&self) -> usize {
        self.metadata.doc_count
    }

    /// Check if the index is empty.
    pub fn is_empty(&self) -> bool {
        self.metadata.doc_count == 0
    }

    /// Get document ID at index.
    pub fn doc_id(&self, idx: usize) -> Option<&DocumentId> {
        self.doc_ids.get(idx)
    }

    /// Get message ID at index.
    pub fn message_id(&self, idx: usize) -> Option<u64> {
        self.message_ids.get(idx).copied()
    }

    /// Get fast embedding at index.
    fn fast_embedding(&self, idx: usize, dim: usize) -> Option<&[f16]> {
        let start = idx * dim;
        let end = start + dim;
        if end <= self.fast_embeddings.len() {
            Some(&self.fast_embeddings[start..end])
        } else {
            None
        }
    }

    /// Get quality embedding at index.
    fn quality_embedding(&self, idx: usize, dim: usize) -> Option<&[f16]> {
        let start = idx * dim;
        let end = start + dim;
        if end <= self.quality_embeddings.len() {
            Some(&self.quality_embeddings[start..end])
        } else {
            None
        }
    }

    /// Search using fast embeddings only.
    pub fn search_fast(&self, query_vec: &[f32], k: usize) -> Vec<ScoredResult> {
        if self.is_empty() || k == 0 {
            return Vec::new();
        }

        let dim = query_vec.len();
        let mut heap = BinaryHeap::with_capacity(k + 1);

        for idx in 0..self.metadata.doc_count {
            if let Some(embedding) = self.fast_embedding(idx, dim) {
                let score = dot_product_f16(embedding, query_vec);
                heap.push(std::cmp::Reverse(ScoredEntry { score, idx }));
                if heap.len() > k {
                    heap.pop();
                }
            }
        }

        heap.into_sorted_vec()
            .into_iter()
            .map(|std::cmp::Reverse(entry)| ScoredResult {
                idx: entry.idx,
                message_id: self.message_ids[entry.idx],
                score: entry.score,
            })
            .collect()
    }

    /// Search using quality embeddings only.
    pub fn search_quality(&self, query_vec: &[f32], k: usize) -> Vec<ScoredResult> {
        if self.is_empty() || k == 0 {
            return Vec::new();
        }

        let dim = query_vec.len();
        let mut heap = BinaryHeap::with_capacity(k + 1);

        for idx in 0..self.metadata.doc_count {
            if let Some(embedding) = self.quality_embedding(idx, dim) {
                let score = dot_product_f16(embedding, query_vec);
                heap.push(std::cmp::Reverse(ScoredEntry { score, idx }));
                if heap.len() > k {
                    heap.pop();
                }
            }
        }

        heap.into_sorted_vec()
            .into_iter()
            .map(|std::cmp::Reverse(entry)| ScoredResult {
                idx: entry.idx,
                message_id: self.message_ids[entry.idx],
                score: entry.score,
            })
            .collect()
    }

    /// Get quality scores for a set of document indices.
    pub fn quality_scores_for_indices(&self, query_vec: &[f32], indices: &[usize]) -> Vec<f32> {
        let dim = query_vec.len();
        indices
            .iter()
            .map(|&idx| {
                self.quality_embedding(idx, dim)
                    .map(|emb| dot_product_f16(emb, query_vec))
                    .unwrap_or(0.0)
            })
            .collect()
    }
}

/// Scored entry for heap-based top-k search.
#[derive(Debug, Clone, Copy)]
struct ScoredEntry {
    score: f32,
    idx: usize,
}

impl PartialEq for ScoredEntry {
    fn eq(&self, other: &Self) -> bool {
        self.score.total_cmp(&other.score) == Ordering::Equal && self.idx == other.idx
    }
}

impl Eq for ScoredEntry {}

impl PartialOrd for ScoredEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScoredEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .total_cmp(&other.score)
            .then_with(|| self.idx.cmp(&other.idx))
    }
}

/// Search result with score and metadata.
#[derive(Debug, Clone)]
pub struct ScoredResult {
    /// Index in the two-tier index.
    pub idx: usize,
    /// Message ID for SQLite lookup.
    pub message_id: u64,
    /// Similarity score.
    pub score: f32,
}

/// Search phase result for progressive display.
#[derive(Debug, Clone)]
pub enum SearchPhase {
    /// Initial fast results.
    Initial {
        results: Vec<ScoredResult>,
        latency_ms: u64,
    },
    /// Refined quality results.
    Refined {
        results: Vec<ScoredResult>,
        latency_ms: u64,
    },
    /// Refinement failed, keep using initial results.
    RefinementFailed { error: String },
}

/// Two-tier searcher that coordinates fast and quality search.
pub struct TwoTierSearcher<'a, D: DaemonClient> {
    index: &'a TwoTierIndex,
    daemon: Option<Arc<D>>,
    fast_embedder: Arc<dyn Embedder>,
    config: TwoTierConfig,
}

impl<'a, D: DaemonClient> TwoTierSearcher<'a, D> {
    /// Create a new two-tier searcher.
    pub fn new(
        index: &'a TwoTierIndex,
        fast_embedder: Arc<dyn Embedder>,
        daemon: Option<Arc<D>>,
        config: TwoTierConfig,
    ) -> Self {
        Self {
            index,
            daemon,
            fast_embedder,
            config,
        }
    }

    /// Perform two-tier progressive search.
    ///
    /// Returns an iterator that yields search phases:
    /// 1. Initial results from fast embeddings
    /// 2. Refined results from quality embeddings (if daemon available)
    pub fn search(&self, query: &str, k: usize) -> impl Iterator<Item = SearchPhase> + '_ {
        TwoTierSearchIter::new(self, query.to_string(), k)
    }

    /// Perform fast-only search (no daemon refinement).
    pub fn search_fast_only(&self, query: &str, k: usize) -> Result<Vec<ScoredResult>> {
        let start = Instant::now();
        let query_vec = self.fast_embedder.embed(query)?;
        let results = self.index.search_fast(&query_vec, k);
        debug!(
            query_len = query.len(),
            k = k,
            result_count = results.len(),
            latency_ms = start.elapsed().as_millis(),
            "Fast-only search completed"
        );
        Ok(results)
    }

    /// Perform quality-only search (wait for daemon).
    pub fn search_quality_only(
        &self,
        query: &str,
        k: usize,
    ) -> Result<Vec<ScoredResult>, TwoTierError> {
        let start = Instant::now();

        let daemon = self
            .daemon
            .as_ref()
            .ok_or_else(|| TwoTierError::DaemonUnavailable("no daemon configured".into()))?;

        if !daemon.is_available() {
            return Err(TwoTierError::DaemonUnavailable(
                "daemon not available".into(),
            ));
        }

        let request_id = format!("quality-{:016x}", rand::random::<u64>());
        let query_vec = daemon
            .embed(query, &request_id)
            .map_err(TwoTierError::DaemonError)?;

        let results = self.index.search_quality(&query_vec, k);
        debug!(
            query_len = query.len(),
            k = k,
            result_count = results.len(),
            latency_ms = start.elapsed().as_millis(),
            "Quality-only search completed"
        );
        Ok(results)
    }
}

/// Iterator for two-tier search phases.
struct TwoTierSearchIter<'a, D: DaemonClient> {
    searcher: &'a TwoTierSearcher<'a, D>,
    query: String,
    k: usize,
    phase: u8,
    fast_results: Option<Vec<ScoredResult>>,
}

impl<'a, D: DaemonClient> TwoTierSearchIter<'a, D> {
    fn new(searcher: &'a TwoTierSearcher<'a, D>, query: String, k: usize) -> Self {
        Self {
            searcher,
            query,
            k,
            phase: 0,
            fast_results: None,
        }
    }
}

impl<'a, D: DaemonClient> Iterator for TwoTierSearchIter<'a, D> {
    type Item = SearchPhase;

    fn next(&mut self) -> Option<Self::Item> {
        match self.phase {
            0 => {
                // Phase 1: Fast search
                self.phase = 1;
                let start = Instant::now();

                match self.searcher.fast_embedder.embed(&self.query) {
                    Ok(query_vec) => {
                        let results = self.searcher.index.search_fast(&query_vec, self.k);
                        let latency_ms = start.elapsed().as_millis() as u64;
                        self.fast_results = Some(results.clone());

                        // If fast-only mode, skip refinement
                        if self.searcher.config.fast_only {
                            self.phase = 2; // Skip refinement
                        }

                        Some(SearchPhase::Initial {
                            results,
                            latency_ms,
                        })
                    }
                    Err(e) => {
                        warn!(error = %e, "Fast embedding failed");
                        self.phase = 2; // Skip to end
                        None
                    }
                }
            }
            1 => {
                // Phase 2: Quality refinement
                self.phase = 2;

                let daemon = match &self.searcher.daemon {
                    Some(d) if d.is_available() => d,
                    _ => {
                        return Some(SearchPhase::RefinementFailed {
                            error: "daemon unavailable".to_string(),
                        });
                    }
                };

                let start = Instant::now();
                let request_id = format!("refine-{:016x}", rand::random::<u64>());

                match daemon.embed(&self.query, &request_id) {
                    Ok(query_vec) => {
                        // Get candidate indices from fast results
                        let candidates: Vec<usize> = self
                            .fast_results
                            .as_ref()
                            .map(|r| r.iter().map(|sr| sr.idx).collect())
                            .unwrap_or_default();

                        // If we have fast results, blend scores; otherwise full quality search
                        let results = if !candidates.is_empty() {
                            let fast_results = self.fast_results.as_ref().unwrap();
                            let quality_scores = self
                                .searcher
                                .index
                                .quality_scores_for_indices(&query_vec, &candidates);

                            // Blend scores
                            let weight = self.searcher.config.quality_weight;
                            let mut blended: Vec<ScoredResult> = fast_results
                                .iter()
                                .zip(quality_scores.iter())
                                .map(|(fast, &quality)| ScoredResult {
                                    idx: fast.idx,
                                    message_id: fast.message_id,
                                    score: (1.0 - weight) * fast.score + weight * quality,
                                })
                                .collect();

                            // Re-sort by blended score
                            blended.sort_by(|a, b| {
                                b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal)
                            });
                            blended.truncate(self.k);
                            blended
                        } else {
                            self.searcher.index.search_quality(&query_vec, self.k)
                        };

                        let latency_ms = start.elapsed().as_millis() as u64;
                        Some(SearchPhase::Refined {
                            results,
                            latency_ms,
                        })
                    }
                    Err(e) => Some(SearchPhase::RefinementFailed {
                        error: e.to_string(),
                    }),
                }
            }
            _ => None,
        }
    }
}

/// Errors specific to two-tier search.
#[derive(Debug, thiserror::Error)]
pub enum TwoTierError {
    #[error("daemon unavailable: {0}")]
    DaemonUnavailable(String),

    #[error("daemon error: {0}")]
    DaemonError(#[from] DaemonError),

    #[error("embedding failed: {0}")]
    EmbeddingFailed(String),

    #[error("index error: {0}")]
    IndexError(String),
}

/// Normalize scores to [0, 1] range.
pub fn normalize_scores(scores: &[f32]) -> Vec<f32> {
    if scores.is_empty() {
        return Vec::new();
    }

    let min = scores.iter().copied().fold(f32::INFINITY, f32::min);
    let max = scores.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let range = max - min;

    if range.abs() < f32::EPSILON {
        return vec![1.0; scores.len()];
    }

    scores.iter().map(|&s| (s - min) / range).collect()
}

/// Blend two score vectors with the given weight for the second vector.
pub fn blend_scores(fast: &[f32], quality: &[f32], quality_weight: f32) -> Vec<f32> {
    let fast_norm = normalize_scores(fast);
    let quality_norm = normalize_scores(quality);

    fast_norm
        .iter()
        .zip(quality_norm.iter())
        .map(|(&f, &q)| (1.0 - quality_weight) * f + quality_weight * q)
        .collect()
}

/// SIMD-accelerated f16 dot product.
///
/// # Panics
/// Debug-asserts that `a.len() == b.len()`. In release mode, mismatched
/// lengths will cause out-of-bounds access if `b` is shorter than `a`.
#[inline]
fn dot_product_f16(a: &[f16], b: &[f32]) -> f32 {
    use wide::f32x8;

    debug_assert_eq!(
        a.len(),
        b.len(),
        "dot_product_f16: dimension mismatch (a={}, b={})",
        a.len(),
        b.len()
    );

    // Early return for mismatched lengths in release mode to avoid UB
    if a.len() != b.len() {
        return 0.0;
    }

    let chunks = a.len() / 8;
    let mut sum = f32x8::ZERO;

    for i in 0..chunks {
        let base = i * 8;
        let a_f32 = [
            f32::from(a[base]),
            f32::from(a[base + 1]),
            f32::from(a[base + 2]),
            f32::from(a[base + 3]),
            f32::from(a[base + 4]),
            f32::from(a[base + 5]),
            f32::from(a[base + 6]),
            f32::from(a[base + 7]),
        ];
        // Safety: We've verified a.len() == b.len() and base + 8 <= chunks * 8 <= a.len()
        let b_arr: [f32; 8] = b[base..base + 8]
            .try_into()
            .expect("slice length mismatch in SIMD chunk");
        sum += f32x8::from(a_f32) * f32x8::from(b_arr);
    }

    let mut result: f32 = sum.reduce_add();

    // Handle remainder - bounds are guaranteed by the length check above
    let remainder_start = chunks * 8;
    for i in remainder_start..a.len() {
        result += f32::from(a[i]) * b[i];
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_entries(count: usize, fast_dim: usize, quality_dim: usize) -> Vec<TwoTierEntry> {
        (0..count)
            .map(|i| TwoTierEntry {
                doc_id: DocumentId::Session(format!("session-{}", i)),
                message_id: i as u64,
                fast_embedding: (0..fast_dim)
                    .map(|j| f16::from_f32((i + j) as f32 * 0.01))
                    .collect(),
                quality_embedding: (0..quality_dim)
                    .map(|j| f16::from_f32((i + j) as f32 * 0.01))
                    .collect(),
            })
            .collect()
    }

    #[test]
    fn test_two_tier_index_creation() {
        let config = TwoTierConfig::default();
        let entries = make_test_entries(10, config.fast_dimension, config.quality_dimension);

        let index = TwoTierIndex::build("fast-256", "quality-384", &config, entries).unwrap();

        assert_eq!(index.len(), 10);
        assert!(!index.is_empty());
        assert!(matches!(
            index.metadata.status,
            IndexStatus::Complete { .. }
        ));
    }

    #[test]
    fn test_empty_index() {
        let config = TwoTierConfig::default();
        let entries: Vec<TwoTierEntry> = Vec::new();

        let index = TwoTierIndex::build("fast-256", "quality-384", &config, entries).unwrap();

        assert_eq!(index.len(), 0);
        assert!(index.is_empty());
    }

    #[test]
    fn test_dimension_mismatch_fast() {
        let config = TwoTierConfig::default();
        let entries = vec![TwoTierEntry {
            doc_id: DocumentId::Session("test".into()),
            message_id: 1,
            fast_embedding: vec![f16::from_f32(1.0); 128], // Wrong dimension
            quality_embedding: vec![f16::from_f32(1.0); config.quality_dimension],
        }];

        let result = TwoTierIndex::build("fast", "quality", &config, entries);
        assert!(result.is_err());
    }

    #[test]
    fn test_dimension_mismatch_quality() {
        let config = TwoTierConfig::default();
        let entries = vec![TwoTierEntry {
            doc_id: DocumentId::Session("test".into()),
            message_id: 1,
            fast_embedding: vec![f16::from_f32(1.0); config.fast_dimension],
            quality_embedding: vec![f16::from_f32(1.0); 128], // Wrong dimension
        }];

        let result = TwoTierIndex::build("fast", "quality", &config, entries);
        assert!(result.is_err());
    }

    #[test]
    fn test_fast_search() {
        let config = TwoTierConfig::default();
        let entries = make_test_entries(100, config.fast_dimension, config.quality_dimension);
        let index = TwoTierIndex::build("fast-256", "quality-384", &config, entries).unwrap();

        let query: Vec<f32> = (0..config.fast_dimension)
            .map(|i| i as f32 * 0.01)
            .collect();
        let results = index.search_fast(&query, 10);

        assert_eq!(results.len(), 10);
        // Results should be sorted by score descending
        for window in results.windows(2) {
            assert!(window[0].score >= window[1].score);
        }
    }

    #[test]
    fn test_quality_search() {
        let config = TwoTierConfig::default();
        let entries = make_test_entries(100, config.fast_dimension, config.quality_dimension);
        let index = TwoTierIndex::build("fast-256", "quality-384", &config, entries).unwrap();

        let query: Vec<f32> = (0..config.quality_dimension)
            .map(|i| i as f32 * 0.01)
            .collect();
        let results = index.search_quality(&query, 10);

        assert_eq!(results.len(), 10);
        // Results should be sorted by score descending
        for window in results.windows(2) {
            assert!(window[0].score >= window[1].score);
        }
    }

    #[test]
    fn test_score_normalization() {
        let scores = vec![0.8, 0.6, 0.4, 0.2];
        let normalized = normalize_scores(&scores);

        assert!((normalized[0] - 1.0).abs() < 0.001);
        assert!((normalized[3] - 0.0).abs() < 0.001);
    }

    #[test]
    fn test_score_normalization_constant() {
        let scores = vec![0.5, 0.5, 0.5];
        let normalized = normalize_scores(&scores);

        // All same value should normalize to 1.0
        for n in &normalized {
            assert!((n - 1.0).abs() < 0.001);
        }
    }

    #[test]
    fn test_score_normalization_empty() {
        let scores: Vec<f32> = vec![];
        let normalized = normalize_scores(&scores);
        assert!(normalized.is_empty());
    }

    #[test]
    fn test_blend_scores() {
        let fast = vec![0.8, 0.6, 0.4];
        let quality = vec![0.4, 0.8, 0.6];
        let blended = blend_scores(&fast, &quality, 0.5);

        assert_eq!(blended.len(), 3);
        // With 0.5 weight, blended should be average of normalized scores
    }

    #[test]
    fn test_document_id_session() {
        let doc_id = DocumentId::Session("test-session".into());
        assert_eq!(doc_id.session_id(), "test-session");
    }

    #[test]
    fn test_document_id_turn() {
        let doc_id = DocumentId::Turn("test-session".into(), 5);
        assert_eq!(doc_id.session_id(), "test-session");
    }

    #[test]
    fn test_document_id_code_block() {
        let doc_id = DocumentId::CodeBlock("test-session".into(), 3, 2);
        assert_eq!(doc_id.session_id(), "test-session");
    }

    #[test]
    fn test_config_defaults() {
        let config = TwoTierConfig::default();
        assert_eq!(config.fast_dimension, 256);
        assert_eq!(config.quality_dimension, 384);
        assert!((config.quality_weight - 0.7).abs() < 0.001);
        assert_eq!(config.max_refinement_docs, 100);
        assert!(!config.fast_only);
        assert!(!config.quality_only);
    }

    #[test]
    fn test_config_fast_only() {
        let config = TwoTierConfig::fast_only();
        assert!(config.fast_only);
        assert!(!config.quality_only);
    }

    #[test]
    fn test_config_quality_only() {
        let config = TwoTierConfig::quality_only();
        assert!(!config.fast_only);
        assert!(config.quality_only);
    }

    #[test]
    fn test_dot_product_f16_basic() {
        let a: Vec<f16> = vec![f16::from_f32(1.0); 8];
        let b: Vec<f32> = vec![1.0; 8];
        let result = dot_product_f16(&a, &b);
        assert!((result - 8.0).abs() < 0.01);
    }

    #[test]
    fn test_dot_product_f16_with_remainder() {
        let a: Vec<f16> = vec![f16::from_f32(1.0); 10];
        let b: Vec<f32> = vec![1.0; 10];
        let result = dot_product_f16(&a, &b);
        assert!((result - 10.0).abs() < 0.01);
    }

    #[test]
    fn test_dot_product_f16_empty() {
        let a: Vec<f16> = vec![];
        let b: Vec<f32> = vec![];
        let result = dot_product_f16(&a, &b);
        assert_eq!(result, 0.0);
    }

    #[test]
    fn test_quality_scores_for_indices() {
        let config = TwoTierConfig::default();
        let entries = make_test_entries(10, config.fast_dimension, config.quality_dimension);
        let index = TwoTierIndex::build("fast-256", "quality-384", &config, entries).unwrap();

        let query: Vec<f32> = (0..config.quality_dimension)
            .map(|i| i as f32 * 0.01)
            .collect();
        let indices = vec![0, 2, 4];
        let scores = index.quality_scores_for_indices(&query, &indices);

        assert_eq!(scores.len(), 3);
    }
}
