//! Two-tier progressive search for session search (bd-3dcw, bd-2fu7e).
//!
//! This module implements a progressive search strategy that:
//! 1. Returns instant results using a fast embedding model (in-process)
//! 2. Refines rankings in the background using a quality model (daemon)
//!
//! **Delegates to frankensearch**: The vector storage and search are backed by
//! `frankensearch_index::TwoTierIndex` (file-backed FSVI). This module adds
//! cass-specific layers: synchronous `Iterator`-based search, `DocumentId`
//! enum, `message_id` for SQLite, and `DaemonClient` integration.
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
//! let index = TwoTierIndex::build("fast", "quality", &config, entries)?;
//! let searcher = TwoTierSearcher::new(&index, fast_embedder, Some(daemon), config);
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
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Result, bail};
use half::f16;
use tracing::{debug, warn};

use super::daemon_client::{DaemonClient, DaemonError};
use super::embedder::Embedder;

// Frankensearch types for vector storage and search delegation.
use frankensearch::TwoTierConfig as FsTwoTierConfig;
use frankensearch::{TwoTierIndex as FsTwoTierIndex, VectorHit as FsVectorHit};

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

    /// Convert to frankensearch TwoTierConfig.
    fn to_fs_config(&self) -> FsTwoTierConfig {
        FsTwoTierConfig {
            quality_weight: f64::from(self.quality_weight),
            fast_only: self.fast_only,
            ..FsTwoTierConfig::optimized().with_env_overrides()
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

    /// Encode as a string for frankensearch doc_id storage.
    fn encode(&self) -> String {
        match self {
            Self::Session(id) => format!("s:{id}"),
            Self::Turn(id, turn) => format!("t:{id}:{turn}"),
            Self::CodeBlock(id, turn, block) => format!("c:{id}:{turn}:{block}"),
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
///
/// Delegates vector storage and search to frankensearch's file-backed FSVI
/// `TwoTierIndex`, with cass-specific side tables for `DocumentId` enum
/// and `message_id` SQLite foreign keys.
#[derive(Debug)]
pub struct TwoTierIndex {
    /// Index metadata.
    pub metadata: TwoTierMetadata,
    /// Frankensearch file-backed two-tier index (None when empty).
    fs_index: Option<FsTwoTierIndex>,
    /// Document IDs in index order (cass-specific enum).
    doc_ids: Vec<DocumentId>,
    /// Message IDs for SQLite lookup (parallel to doc_ids).
    message_ids: Vec<u64>,
    /// Temp directory holding FSVI files (kept alive for index lifetime).
    _tmpdir: Option<tempfile::TempDir>,
}

impl TwoTierIndex {
    /// Build a two-tier index from entries.
    ///
    /// Creates a temporary FSVI index via frankensearch's `TwoTierIndexBuilder`,
    /// then opens it for search. The temp directory is kept alive as long as the
    /// index exists.
    pub fn build(
        fast_embedder_id: impl Into<String>,
        quality_embedder_id: impl Into<String>,
        config: &TwoTierConfig,
        entries: impl IntoIterator<Item = TwoTierEntry>,
    ) -> Result<Self> {
        let fast_embedder_id = fast_embedder_id.into();
        let quality_embedder_id = quality_embedder_id.into();
        let entries: Vec<TwoTierEntry> = entries.into_iter().collect();
        let doc_count = entries.len();

        let tmpdir = tempfile::TempDir::new()?;

        if doc_count == 0 {
            return Ok(Self {
                metadata: TwoTierMetadata {
                    fast_embedder_id,
                    quality_embedder_id,
                    doc_count: 0,
                    built_at: chrono::Utc::now().timestamp(),
                    status: IndexStatus::Complete {
                        fast_latency_ms: 0,
                        quality_latency_ms: 0,
                    },
                },
                fs_index: None,
                doc_ids: Vec::new(),
                message_ids: Vec::new(),
                _tmpdir: None,
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

        // Build frankensearch index
        let fs_config = config.to_fs_config();
        let mut builder = FsTwoTierIndex::create(tmpdir.path(), fs_config.clone())
            .map_err(|e| anyhow::anyhow!("failed to create fs index builder: {e}"))?;
        builder.set_fast_embedder_id(&fast_embedder_id);
        builder.set_quality_embedder_id(&quality_embedder_id);

        let mut doc_ids = Vec::with_capacity(doc_count);
        let mut message_ids = Vec::with_capacity(doc_count);

        for entry in entries {
            let doc_id_str = entry.doc_id.encode();
            let fast_f32: Vec<f32> = entry.fast_embedding.iter().map(|v| f32::from(*v)).collect();
            let quality_f32: Vec<f32> = entry
                .quality_embedding
                .iter()
                .map(|v| f32::from(*v))
                .collect();

            builder
                .add_record(&doc_id_str, &fast_f32, Some(&quality_f32))
                .map_err(|e| anyhow::anyhow!("failed to add record {doc_id_str}: {e}"))?;
            doc_ids.push(entry.doc_id);
            message_ids.push(entry.message_id);
        }

        let fs_index = builder
            .finish()
            .map_err(|e| anyhow::anyhow!("failed to finish fs index: {e}"))?;

        Ok(Self {
            metadata: TwoTierMetadata {
                fast_embedder_id,
                quality_embedder_id,
                doc_count,
                built_at: chrono::Utc::now().timestamp(),
                status: IndexStatus::Complete {
                    fast_latency_ms: 0,
                    quality_latency_ms: 0,
                },
            },
            fs_index: Some(fs_index),
            doc_ids,
            message_ids,
            _tmpdir: Some(tmpdir),
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

    /// Search using fast embeddings only.
    ///
    /// Delegates to frankensearch's `TwoTierIndex::search_fast()`.
    pub fn search_fast(&self, query_vec: &[f32], k: usize) -> Vec<ScoredResult> {
        if self.is_empty() || k == 0 {
            return Vec::new();
        }

        let Some(fs_index) = &self.fs_index else {
            return Vec::new();
        };

        match fs_index.search_fast(query_vec, k) {
            Ok(hits) => self.hits_to_scored_results(hits),
            Err(e) => {
                warn!(error = %e, "frankensearch fast search failed");
                Vec::new()
            }
        }
    }

    /// Search using quality embeddings only.
    ///
    /// Delegates to frankensearch's quality search via `search_fast` on the
    /// quality index. Since frankensearch's `TwoTierIndex` stores both tiers,
    /// we use `quality_scores_for_hits` with all documents as candidates.
    pub fn search_quality(&self, query_vec: &[f32], k: usize) -> Vec<ScoredResult> {
        if self.is_empty() || k == 0 {
            return Vec::new();
        }

        let Some(fs_index) = &self.fs_index else {
            return Vec::new();
        };

        // Build candidate hits for all docs to get quality scores
        let all_hits: Vec<FsVectorHit> = (0..self.metadata.doc_count)
            .map(|i| FsVectorHit {
                index: i as u32,
                score: 0.0,
                doc_id: self.doc_ids[i].encode(),
            })
            .collect();

        match fs_index.quality_scores_for_hits(query_vec, &all_hits) {
            Ok(scores) => {
                // Build scored results and sort by score descending
                let mut results: Vec<ScoredResult> = scores
                    .iter()
                    .enumerate()
                    .map(|(idx, &score)| ScoredResult {
                        idx,
                        message_id: self.message_ids[idx],
                        score,
                    })
                    .collect();
                results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
                results.truncate(k);
                results
            }
            Err(e) => {
                warn!(error = %e, "frankensearch quality search failed");
                Vec::new()
            }
        }
    }

    /// Get quality scores for a set of document indices.
    pub fn quality_scores_for_indices(&self, query_vec: &[f32], indices: &[usize]) -> Vec<f32> {
        let Some(fs_index) = &self.fs_index else {
            return vec![0.0; indices.len()];
        };

        let hits: Vec<FsVectorHit> = indices
            .iter()
            .filter_map(|&idx| {
                if idx < self.metadata.doc_count {
                    Some(FsVectorHit {
                        index: idx as u32,
                        score: 0.0,
                        doc_id: self.doc_ids[idx].encode(),
                    })
                } else {
                    None
                }
            })
            .collect();

        match fs_index.quality_scores_for_hits(query_vec, &hits) {
            Ok(scores) => scores,
            Err(e) => {
                warn!(error = %e, "frankensearch quality scoring failed; using zero scores");
                vec![0.0; indices.len()]
            }
        }
    }

    /// Convert frankensearch VectorHits to cass ScoredResults.
    fn hits_to_scored_results(&self, hits: Vec<FsVectorHit>) -> Vec<ScoredResult> {
        hits.into_iter()
            .filter_map(|hit| {
                let idx = hit.index as usize;
                if idx < self.metadata.doc_count {
                    Some(ScoredResult {
                        idx,
                        message_id: self.message_ids[idx],
                        score: hit.score,
                    })
                } else {
                    None
                }
            })
            .collect()
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
    /// Initial results from fast embeddings.
    Initial {
        results: Vec<ScoredResult>,
        latency_ms: u64,
    },
    /// Refined results from quality embeddings (if daemon available).
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
        let query_vec = self.fast_embedder.embed_sync(query)?;
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
                if self.searcher.config.quality_only {
                    self.phase = 2;
                    let start = Instant::now();
                    return match self.searcher.search_quality_only(&self.query, self.k) {
                        Ok(results) => Some(SearchPhase::Refined {
                            results,
                            latency_ms: start.elapsed().as_millis() as u64,
                        }),
                        Err(e) => Some(SearchPhase::RefinementFailed {
                            error: e.to_string(),
                        }),
                    };
                }

                // Phase 1: Fast search
                self.phase = 1;
                let start = Instant::now();

                match self.searcher.fast_embedder.embed_sync(&self.query) {
                    Ok(query_vec) => {
                        let results = self.searcher.index.search_fast(&query_vec, self.k);
                        let latency_ms = start.elapsed().as_millis() as u64;
                        self.fast_results = Some(results.clone());

                        if self.searcher.config.fast_only {
                            self.phase = 2;
                        }

                        Some(SearchPhase::Initial {
                            results,
                            latency_ms,
                        })
                    }
                    Err(e) => {
                        warn!(error = %e, "Fast embedding failed");
                        self.phase = 2;
                        Some(SearchPhase::RefinementFailed {
                            error: format!("fast embedding failed: {e}"),
                        })
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
                        let results = if let Some(fast_results) = self.fast_results.as_ref() {
                            let refine_cap = self.searcher.config.max_refinement_docs;
                            let candidates: Vec<usize> = fast_results
                                .iter()
                                .take(refine_cap)
                                .map(|sr| sr.idx)
                                .collect();
                            if candidates.is_empty() {
                                fast_results.clone()
                            } else {
                                let quality_scores = self
                                    .searcher
                                    .index
                                    .quality_scores_for_indices(&query_vec, &candidates);

                                let weight = self.searcher.config.quality_weight;
                                let fast_scores: Vec<f32> =
                                    fast_results.iter().map(|sr| sr.score).collect();
                                let fast_norm = normalize_scores(&fast_scores);
                                let quality_norm = normalize_scores(&quality_scores);

                                let mut blended: Vec<ScoredResult> =
                                    Vec::with_capacity(fast_results.len());
                                for (idx, fast) in fast_results.iter().enumerate() {
                                    let score = if idx < quality_norm.len() {
                                        let fast_s = fast_norm.get(idx).copied().unwrap_or(0.0);
                                        let quality_s = quality_norm[idx];
                                        (1.0 - weight) * fast_s + weight * quality_s
                                    } else {
                                        // Unrefined documents get a penalized score that assumes 0.0 for quality
                                        // to preserve their original ranking but place them appropriately below
                                        // high-quality refined items.
                                        fast_norm.get(idx).copied().unwrap_or(0.0) * (1.0 - weight)
                                    };
                                    blended.push(ScoredResult {
                                        idx: fast.idx,
                                        message_id: fast.message_id,
                                        score,
                                    });
                                }

                                blended.sort_by(|a, b| {
                                    b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal)
                                });
                                blended.truncate(self.k);
                                blended
                            }
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

    let valid_scores = scores.iter().copied().filter(|s| !s.is_nan());
    let min = valid_scores.clone().fold(f32::INFINITY, f32::min);
    let max = valid_scores.fold(f32::NEG_INFINITY, f32::max);

    if min.is_infinite() || max.is_infinite() {
        return vec![0.0; scores.len()];
    }

    let range = max - min;

    if range.abs() < f32::EPSILON {
        return scores
            .iter()
            .map(|&s| if s.is_nan() { 0.0 } else { 1.0 })
            .collect();
    }

    scores
        .iter()
        .map(|&s| if s.is_nan() { 0.0 } else { (s - min) / range })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::daemon_client::{DaemonClient, DaemonError};
    use crate::search::embedder::{Embedder, EmbedderError};
    use crate::search::hash_embedder::HashEmbedder;
    use frankensearch::ModelCategory;
    use std::sync::Arc;

    struct TestDaemon {
        dim: usize,
        available: bool,
    }

    struct FailingEmbedder {
        dim: usize,
    }

    struct ConstantEmbedder {
        dim: usize,
        value: f32,
    }

    impl Embedder for FailingEmbedder {
        fn embed_sync(&self, _text: &str) -> Result<Vec<f32>, EmbedderError> {
            Err(EmbedderError::EmbeddingFailed {
                model: "failing-embedder".to_string(),
                source: Box::new(std::io::Error::other("synthetic fast embed failure")),
            })
        }

        fn dimension(&self) -> usize {
            self.dim
        }

        fn id(&self) -> &str {
            "failing-embedder"
        }

        fn is_semantic(&self) -> bool {
            false
        }

        fn category(&self) -> ModelCategory {
            ModelCategory::HashEmbedder
        }
    }

    impl Embedder for ConstantEmbedder {
        fn embed_sync(&self, _text: &str) -> Result<Vec<f32>, EmbedderError> {
            Ok(vec![self.value; self.dim])
        }

        fn dimension(&self) -> usize {
            self.dim
        }

        fn id(&self) -> &str {
            "constant-embedder"
        }

        fn is_semantic(&self) -> bool {
            false
        }

        fn category(&self) -> ModelCategory {
            ModelCategory::HashEmbedder
        }
    }

    impl DaemonClient for TestDaemon {
        fn id(&self) -> &str {
            "test-daemon"
        }

        fn is_available(&self) -> bool {
            self.available
        }

        fn embed(&self, _text: &str, _request_id: &str) -> Result<Vec<f32>, DaemonError> {
            Ok(vec![1.0; self.dim])
        }

        fn embed_batch(
            &self,
            texts: &[&str],
            _request_id: &str,
        ) -> Result<Vec<Vec<f32>>, DaemonError> {
            Ok(vec![vec![1.0; self.dim]; texts.len()])
        }

        fn rerank(
            &self,
            _query: &str,
            _documents: &[&str],
            _request_id: &str,
        ) -> Result<Vec<f32>, DaemonError> {
            Err(DaemonError::Unavailable(
                "rerank unsupported in test daemon".to_string(),
            ))
        }
    }

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

        for n in &normalized {
            assert!((n - 1.0).abs() < 0.001);
        }
    }

    #[test]
    fn test_score_normalization_constant_with_nan_keeps_nan_zeroed() {
        let scores = vec![f32::NAN, 0.5, 0.5];
        let normalized = normalize_scores(&scores);

        assert_eq!(normalized.len(), 3);
        assert_eq!(normalized[0], 0.0);
        assert!((normalized[1] - 1.0).abs() < 0.001);
        assert!((normalized[2] - 1.0).abs() < 0.001);
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

    #[test]
    fn test_search_fast_dimension_mismatch_returns_empty() {
        let config = TwoTierConfig::default();
        let entries = make_test_entries(5, config.fast_dimension, config.quality_dimension);
        let index = TwoTierIndex::build("fast-256", "quality-384", &config, entries).unwrap();

        let bad_query = vec![0.5; config.fast_dimension.saturating_sub(1)];
        let results = index.search_fast(&bad_query, 5);
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_quality_dimension_mismatch_returns_empty() {
        let config = TwoTierConfig::default();
        let entries = make_test_entries(5, config.fast_dimension, config.quality_dimension);
        let index = TwoTierIndex::build("fast-256", "quality-384", &config, entries).unwrap();

        let bad_query = vec![0.5; config.quality_dimension.saturating_sub(1)];
        let results = index.search_quality(&bad_query, 5);
        assert!(results.is_empty());
    }

    #[test]
    fn test_quality_scores_for_indices_dimension_mismatch_returns_zeros() {
        let config = TwoTierConfig::default();
        let entries = make_test_entries(5, config.fast_dimension, config.quality_dimension);
        let index = TwoTierIndex::build("fast-256", "quality-384", &config, entries).unwrap();

        let bad_query = vec![0.5; config.quality_dimension.saturating_sub(1)];
        let scores = index.quality_scores_for_indices(&bad_query, &[0, 2, 4]);
        assert_eq!(scores, vec![0.0, 0.0, 0.0]);
    }

    #[test]
    fn test_quality_only_mode_emits_only_refined_phase() {
        let config = TwoTierConfig {
            fast_dimension: 8,
            quality_dimension: 8,
            quality_only: true,
            ..Default::default()
        };
        let entries = make_test_entries(4, config.fast_dimension, config.quality_dimension);
        let index = TwoTierIndex::build("fast-8", "quality-8", &config, entries).unwrap();

        let fast_embedder: Arc<dyn Embedder> = Arc::new(HashEmbedder::new(config.fast_dimension));
        let daemon = Arc::new(TestDaemon {
            dim: config.quality_dimension,
            available: true,
        });
        let searcher = TwoTierSearcher::new(&index, fast_embedder, Some(daemon), config);
        let phases: Vec<SearchPhase> = searcher.search("query", 3).collect();

        assert_eq!(phases.len(), 1);
        assert!(matches!(phases[0], SearchPhase::Refined { .. }));
    }

    #[test]
    fn test_quality_only_mode_without_daemon_reports_failure() {
        let config = TwoTierConfig {
            fast_dimension: 8,
            quality_dimension: 8,
            quality_only: true,
            ..Default::default()
        };
        let entries = make_test_entries(4, config.fast_dimension, config.quality_dimension);
        let index = TwoTierIndex::build("fast-8", "quality-8", &config, entries).unwrap();

        let fast_embedder: Arc<dyn Embedder> = Arc::new(HashEmbedder::new(config.fast_dimension));
        let daemon = Arc::new(TestDaemon {
            dim: config.quality_dimension,
            available: false,
        });
        let searcher = TwoTierSearcher::new(&index, fast_embedder, Some(daemon), config);
        let phases: Vec<SearchPhase> = searcher.search("query", 3).collect();

        assert_eq!(phases.len(), 1);
        assert!(matches!(phases[0], SearchPhase::RefinementFailed { .. }));
    }

    #[test]
    fn test_fast_embedding_failure_yields_failure_phase() {
        let config = TwoTierConfig {
            fast_dimension: 8,
            quality_dimension: 8,
            fast_only: false,
            quality_only: false,
            ..Default::default()
        };
        let entries = make_test_entries(4, config.fast_dimension, config.quality_dimension);
        let index = TwoTierIndex::build("fast-8", "quality-8", &config, entries).unwrap();

        let fast_embedder: Arc<dyn Embedder> = Arc::new(FailingEmbedder {
            dim: config.fast_dimension,
        });
        let daemon = Arc::new(TestDaemon {
            dim: config.quality_dimension,
            available: true,
        });
        let searcher = TwoTierSearcher::new(&index, fast_embedder, Some(daemon), config);
        let phases: Vec<SearchPhase> = searcher.search("query", 3).collect();

        assert_eq!(phases.len(), 1);
        assert!(matches!(phases[0], SearchPhase::RefinementFailed { .. }));
    }

    #[test]
    fn test_refinement_scores_are_normalized() {
        let config = TwoTierConfig {
            fast_dimension: 8,
            quality_dimension: 8,
            quality_weight: 0.6,
            max_refinement_docs: 3,
            ..Default::default()
        };
        let entries: Vec<TwoTierEntry> = (0..5)
            .map(|i| TwoTierEntry {
                doc_id: DocumentId::Session(format!("s{i}")),
                message_id: i as u64 + 1,
                fast_embedding: vec![f16::from_f32(20.0 + i as f32); config.fast_dimension],
                quality_embedding: vec![f16::from_f32(10.0 + i as f32); config.quality_dimension],
            })
            .collect();
        let index = TwoTierIndex::build("fast-8", "quality-8", &config, entries).unwrap();

        let fast_embedder: Arc<dyn Embedder> = Arc::new(ConstantEmbedder {
            dim: config.fast_dimension,
            value: 10.0,
        });
        let daemon = Arc::new(TestDaemon {
            dim: config.quality_dimension,
            available: true,
        });
        let searcher = TwoTierSearcher::new(&index, fast_embedder, Some(daemon), config);
        let phases: Vec<SearchPhase> = searcher.search("query", 5).collect();

        assert_eq!(phases.len(), 2);
        let SearchPhase::Refined { results, .. } = &phases[1] else {
            panic!("expected refined phase");
        };
        assert!(
            results.iter().all(|r| (0.0..=1.0).contains(&r.score)),
            "expected normalized refined scores, got {:?}",
            results.iter().map(|r| r.score).collect::<Vec<_>>()
        );
    }
}
