//! Optional native HNSW execution over the reader's already-admitted owners.
//!
//! Only explicitly selected, receipted graphs are loaded. Failure never builds
//! or repairs a graph and never removes its exact shard. These limits bound the
//! ANN candidate window, not the engine's visited set, loaded graph, or total RSS.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use frankensearch::core::filter::SearchFilter;
use frankensearch::core::{BoundQueryEmbedding, VectorHit};
use frankensearch::index::native_hnsw::{
    NativeHnswGenerationReceiptV2, ValidatedNativeHnsw,
    native_hnsw_generation_receipt_path,
};
use frankensearch::index::{ValidatedFsviBytes, dot_product_f32_f32};
use serde::Serialize;

use super::{SemanticGenerationReader, SemanticReaderError, SemanticReaderResult, TierKind};

/// A graph and the complete receipt selected by the publication layer. Merely
/// finding a graph beside an FSVI is not sufficient to populate this value.
#[derive(Debug, Clone)]
pub struct SemanticAnnExpectation {
    pub graph_path: PathBuf,
    pub receipt: NativeHnswGenerationReceiptV2,
}

/// Stable reasons for using the retained exact shard instead of its ANN graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AnnFallbackReason {
    NotSelected,
    InvalidExpectation,
    UnsafePath,
    SidecarUnavailable,
    ReceiptMismatch,
    CandidateLimit,
    FilterUnderfill,
    QueryFailed,
}

/// Admission is not query execution. A loaded graph may still be bypassed by
/// an exact request, a zero-k request, or a candidate-limit fallback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SemanticAnnAdmission {
    Unavailable { reason: AnnFallbackReason },
    Admitted { graph_sha256: String, receipt_sha256: String },
}

/// Exact remains the default. Opt-in ANN requests start with this candidate
/// window and double it when filtering underfills. At the cap, that shard uses
/// exact search rather than silently returning a truncated filtered window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnnSearchPolicy {
    pub initial_candidates: usize,
    pub max_candidates: usize,
}

impl Default for AnnSearchPolicy {
    fn default() -> Self {
        Self { initial_candidates: 128, max_candidates: 4096 }
    }
}

impl AnnSearchPolicy {
    pub(super) fn validate(self) -> SemanticReaderResult<()> {
        if self.initial_candidates == 0
            || self.initial_candidates > self.max_candidates
            || self.max_candidates > 65_536
        {
            return Err(SemanticReaderError::InvalidAnnPolicy);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticShardEngine { Skipped, Exact, NativeAnn, ExactFallback }

/// Actual work for one requested shard. `candidate_rows` counts candidates in
/// successfully processed ANN windows, including repeated rows during widening;
/// a failed window is not measured. `ann_windows` counts attempted windows.
/// Neither counts graph-node visits. No timing, recall, or total-RSS claim is made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SemanticShardExecution {
    pub tier: TierKind,
    pub shard: usize,
    pub engine: SemanticShardEngine,
    pub fallback_reason: Option<AnnFallbackReason>,
    pub graph_sha256: Option<String>,
    pub ann_windows: usize,
    pub candidate_rows: usize,
    pub final_candidate_limit: usize,
    pub returned_candidates: usize,
}

#[derive(Debug)]
pub(super) struct AdmittedAnn {
    graph: ValidatedNativeHnsw,
    receipt: NativeHnswGenerationReceiptV2,
}

#[derive(Debug)]
pub(super) enum ShardAnn {
    Unavailable(AnnFallbackReason),
    Ready(Box<AdmittedAnn>),
}

impl ShardAnn {
    fn load(owner: Arc<ValidatedFsviBytes>, expected: Option<&SemanticAnnExpectation>) -> Self {
        let Some(expected) = expected else {
            return Self::Unavailable(AnnFallbackReason::NotSelected);
        };
        // Reject internally invalid or foreign expectations before sidecar I/O.
        // The whole-image digest binds every owner byte; the upstream loader
        // separately checks every persisted identity and topology component.
        if expected.receipt.validate().is_err()
            || expected.receipt.fsvi_whole_image_sha256 != hex::encode(owner.witness().whole_image_sha256)
            || expected.receipt.artifact_generation != owner.witness().generation
        {
            return Self::Unavailable(AnnFallbackReason::InvalidExpectation);
        }
        let graph_path = if expected.graph_path.is_absolute() {
            expected.graph_path.clone()
        } else {
            let Ok(root) = std::env::current_dir() else {
                return Self::Unavailable(AnnFallbackReason::SidecarUnavailable);
            };
            root.join(&expected.graph_path)
        };
        let Ok(receipt_path) = native_hnsw_generation_receipt_path(&graph_path) else {
            return Self::Unavailable(AnnFallbackReason::UnsafePath);
        };
        // Read-only preflight, not a race-free filesystem or allocation budget.
        // Cryptographic/structural admission below remains authoritative.
        if !single_link_regular(&graph_path) || !single_link_regular(&receipt_path) {
            return Self::Unavailable(AnnFallbackReason::SidecarUnavailable);
        }
        match ValidatedNativeHnsw::load(owner, &graph_path) {
            Ok((graph, receipt)) if receipt == expected.receipt => {
                Self::Ready(Box::new(AdmittedAnn { graph, receipt }))
            }
            Ok(_) => Self::Unavailable(AnnFallbackReason::ReceiptMismatch),
            Err(_) => Self::Unavailable(AnnFallbackReason::SidecarUnavailable),
        }
    }

    fn admission(&self) -> SemanticAnnAdmission {
        match self {
            Self::Unavailable(reason) => SemanticAnnAdmission::Unavailable { reason: *reason },
            Self::Ready(ann) => SemanticAnnAdmission::Admitted {
                graph_sha256: ann.receipt.graph_sha256.clone(),
                receipt_sha256: ann.receipt.receipt_sha256.clone(),
            },
        }
    }
}

fn single_link_regular(path: &Path) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else { return false; };
    if !metadata.is_file() { return false; }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 { return false; }
    }
    true
}

#[derive(Debug, Default)]
pub(super) struct AnnSelection {
    fast: Vec<ShardAnn>,
    quality: Vec<ShardAnn>,
}

impl AnnSelection {
    pub(super) fn shard(&self, kind: TierKind, shard: usize) -> Option<&ShardAnn> {
        match kind { TierKind::Fast => self.fast.get(shard), TierKind::Quality => self.quality.get(shard) }
    }
}

impl SemanticGenerationReader {
    /// Attach optional graphs to this exact selection without reopening FSVI.
    /// Supplied lists must align exactly with the selected tier's shard order.
    /// `None` means no graphs selected for that tier; individual `None` entries
    /// explicitly choose exact fallback for those shards. Load failures are
    /// reported by ann_admission(), never promoted to successful ANN admission.
    ///
    /// Consumes this reader value but does not alter existing clones or batches.
    /// No graph build/save, model acquisition, or canonical DB access occurs.
    pub fn with_ann(
        mut self,
        fast: Option<&[Option<SemanticAnnExpectation>]>,
        quality: Option<&[Option<SemanticAnnExpectation>]>,
    ) -> SemanticReaderResult<Self> {
        for (kind, selected) in [(TierKind::Fast, fast), (TierKind::Quality, quality)] {
            if let Some(selected) = selected {
                let tier = self.tier(kind).ok_or(SemanticReaderError::MissingTier(kind))?;
                if selected.len() != tier.shards.len() {
                    return Err(SemanticReaderError::AnnSelectionMismatch(kind));
                }
            }
        }
        let load_tier = |kind, selected: Option<&[Option<SemanticAnnExpectation>]>| {
            self.tier(kind).map_or_else(Vec::new, |tier| {
                tier.shards.iter().enumerate().map(|(position, owner)| {
                    ShardAnn::load(Arc::clone(owner), selected.and_then(|list| list[position].as_ref()))
                }).collect()
            })
        };
        let selected = AnnSelection {
            fast: load_tier(TierKind::Fast, fast),
            quality: load_tier(TierKind::Quality, quality),
        };
        self.ann = Arc::new(selected);
        Ok(self)
    }

    /// Missing tier/shard is None, distinct from an exact shard without ANN.
    pub fn ann_admission(&self, tier: TierKind, shard: usize) -> Option<SemanticAnnAdmission> {
        self.tier(tier)?.shards.get(shard)?;
        Some(self.ann.shard(tier, shard).map_or(
            SemanticAnnAdmission::Unavailable { reason: AnnFallbackReason::NotSelected },
            ShardAnn::admission,
        ))
    }
}

/// One shard execution. The caller has already activated every requested tier.
/// All fallback paths search this owner; no other shard or generation is used.
pub(super) fn search_shard(
    owner: &ValidatedFsviBytes,
    ann: Option<&ShardAnn>,
    query: &BoundQueryEmbedding,
    k: usize,
    filter: Option<&dyn SearchFilter>,
    policy: Option<AnnSearchPolicy>,
    mut report: SemanticShardExecution,
) -> SemanticReaderResult<(Vec<VectorHit>, SemanticShardExecution)> {
    let target = k.min(owner.live_count());
    if target == 0 {
        report.engine = SemanticShardEngine::Skipped;
        return Ok((Vec::new(), report));
    }
    let Some(policy) = policy else {
        let hits = owner.search_top_k(query.vector(), target, filter)?;
        report.engine = SemanticShardEngine::Exact;
        report.returned_candidates = hits.len();
        return Ok((hits, report));
    };
    let ready = match ann {
        Some(ShardAnn::Ready(ready)) => Some(ready),
        Some(ShardAnn::Unavailable(reason)) => { report.fallback_reason = Some(*reason); None }
        None => { report.fallback_reason = Some(AnnFallbackReason::NotSelected); None }
    };
    if let Some(ready) = ready {
        if target > policy.max_candidates {
            report.fallback_reason = Some(AnnFallbackReason::CandidateLimit);
        } else {
            let cap = policy.max_candidates.min(owner.record_count());
            let mut width = policy.initial_candidates.max(target).min(cap);
            loop {
                report.ann_windows += 1;
                report.final_candidate_limit = width;
                let window = native_window(owner, &ready.graph, query, width, filter);
                match window {
                    Ok((mut hits, candidate_rows)) => {
                        report.candidate_rows += candidate_rows;
                        if hits.len() >= target {
                            hits.truncate(target);
                            report.engine = SemanticShardEngine::NativeAnn;
                            report.graph_sha256 = Some(ready.receipt.graph_sha256.clone());
                            report.returned_candidates = hits.len();
                            return Ok((hits, report));
                        }
                        // Even a full-width ANN window uses exact fallback on
                        // underfill, keeping filtered exhaustion and tie rules
                        // in the canonical exact engine rather than guessing.
                        if width == cap {
                            report.fallback_reason = Some(AnnFallbackReason::FilterUnderfill);
                            break;
                        }
                        width = width.saturating_mul(2).max(width + 1).min(cap);
                    }
                    Err(_) => {
                        report.fallback_reason = Some(AnnFallbackReason::QueryFailed);
                        break;
                    }
                }
            }
        }
    }
    let hits = owner.search_top_k(query.vector(), target, filter)?;
    report.engine = SemanticShardEngine::ExactFallback;
    report.returned_candidates = hits.len();
    Ok((hits, report))
}

fn native_window(
    owner: &ValidatedFsviBytes,
    graph: &ValidatedNativeHnsw,
    query: &BoundQueryEmbedding,
    width: usize,
    filter: Option<&dyn SearchFilter>,
) -> SemanticReaderResult<(Vec<VectorHit>, usize)> {
    let candidates = graph.search(query.vector(), width, Some(width))?;
    if candidates.len() > width { return Err(SemanticReaderError::InvalidCandidate); }
    let count = candidates.len();
    let mut hits = Vec::with_capacity(count);
    let mut seen = HashSet::with_capacity(count);
    for candidate in candidates {
        let row = usize::try_from(candidate.physical_row())
            .map_err(|_| SemanticReaderError::InvalidCandidate)?;
        if !seen.insert(row) || !candidate.flags().is_live()
            || owner.doc_id_at(row)? != candidate.doc_id()
        {
            return Err(SemanticReaderError::InvalidCandidate);
        }
        if filter.is_some_and(|filter| !filter.matches(candidate.doc_id(), None)) { continue; }
        // 1.0 - distance loses small scores to rounding. Recompute from the
        // exact retained source at its declared storage precision instead.
        let score = dot_product_f32_f32(&owner.vector_at_f32(row)?, query.vector())?;
        if !score.is_finite() { return Err(SemanticReaderError::InvalidCandidate); }
        hits.push(VectorHit { index: candidate.physical_row(), score, doc_id: candidate.doc_id().into() });
    }
    hits.sort_unstable_by(|left, right| right.score.total_cmp(&left.score)
        .then_with(|| left.index.cmp(&right.index)));
    Ok((hits, count))
