//! Complete retained-WAL selection when the allocation-free fast path declines.
//!
//! An empty post-top-k batch is not an exhaustion certificate: superseded main
//! rows can occupy every raw slot. Scan current rows before selecting messages.
//! Keep the optimized scorer's width/WAL admission rules unchanged; this lane
//! handles its unsupported cases rather than falling through to that shortcut.

use super::{FsSearchFilter, FsVectorIndex, Selection, SemanticIndexArtifact, VectorSearchResult,
    Winner, parse_semantic_doc_id};
use anyhow::{Context, Result, ensure};
use frankensearch::index::{Quantization, dot_product_f16_bytes_f32,
    dot_product_f32_bytes_f32, dot_product_f32_f32};

#[cfg(test)]
mod tests;

// One shard's borrowed references, NOT copies of its strings or vectors. The
// old optimized lane remains capped at 4,096 keys. This cold correctness lane
// can admit millions of retained updates, but still refuses excessive scratch
// explicitly instead of turning a resource limit into a false-empty answer.
const MAX_SHADOW_REFERENCE_BYTES: usize = 64 * 1024 * 1024;

struct ShadowIds<'a> {
    ids: Vec<&'a str>,
}

impl<'a> ShadowIds<'a> {
    fn new(index: &'a FsVectorIndex) -> Result<Self> {
        Self::with_budget(index, MAX_SHADOW_REFERENCE_BYTES)
    }

    fn with_budget(index: &'a FsVectorIndex, budget: usize) -> Result<Self> {
        let count = index.wal_record_count();
        let bytes = count.checked_mul(std::mem::size_of::<&str>())
            .context("retained semantic WAL shadow-reference size overflow")?;
        ensure!(bytes <= budget,
            "retained semantic WAL requires {bytes} bytes of shadow references, exceeding the {budget}-byte query budget; complete 'cass index --semantic' to compact the retained delta, then retry");
        let mut ids = Vec::new();
        ids.try_reserve_exact(count)
            .context("cannot reserve bounded retained semantic WAL shadow references")?;
        ids.extend(index.wal_records().map(|(id, _)| id));
        ensure!(ids.len() == count, "retained semantic WAL changed during admission");
        ids.sort_unstable();
        ids.dedup();
        Ok(Self { ids })
    }

    fn contains(&self, id: &str) -> bool {
        self.ids.binary_search(&id).is_ok()
    }
}

/// Reconstruct the exact stored row for the backend's byte-based dot kernel.
/// `dot_query_at` uses a different tail reduction at non-multiples of 32, so
/// calling it at those widths can change score bits and the ranking of ties.
/// Every finite F16 value has an exact F32 representation; converting it back
/// preserves the stored half (including subnormals and signed zero). F32 bytes
/// are restored directly. One decoded row and one reusable byte row are live.
struct RowScorer {
    wire: Vec<u8>,
}

impl RowScorer {
    fn new() -> Self {
        Self { wire: Vec::new() }
    }

    fn score(&mut self, index: &FsVectorIndex, row: usize, query: &[f32]) -> Result<f32> {
        if index.dimension().is_multiple_of(32) {
            return Ok(index.dot_query_at(row, query)?);
        }
        let vector = index.vector_at_f32(row)?;
        ensure!(vector.len() == query.len(), "retained semantic row dimension mismatch");
        ensure!(vector.iter().all(|value| value.is_finite()),
            "retained semantic row contains a non-finite vector component");
        let width = match index.quantization() {
            Quantization::F16 => 2,
            Quantization::F32 => 4,
        };
        let bytes = vector.len().checked_mul(width)
            .context("retained semantic row byte size overflow")?;
        self.wire.clear();
        self.wire.try_reserve(bytes)
            .context("cannot reserve retained semantic row scoring scratch")?;
        match index.quantization() {
            Quantization::F16 => {
                for value in vector {
                    self.wire.extend_from_slice(&half::f16::from_f32(value).to_le_bytes());
                }
                Ok(dot_product_f16_bytes_f32(&self.wire, query)?)
            }
            Quantization::F32 => {
                for value in vector {
                    self.wire.extend_from_slice(&value.to_le_bytes());
                }
                Ok(dot_product_f32_bytes_f32(&self.wire, query)?)
            }
        }
    }
}

/// Exact current-message selection over the complete already-opened cohort.
/// No limit-induced `None`, raw-window exhaustion inference, file reopen,
/// artifact mutation, model load, or document-text hydration occurs here.
///
/// Selection is O(k); borrowed shadow references are bounded per shard and are
/// dropped before the next shard. Nonstandard widths additionally retain one
/// decoded vector plus its wire representation, never an entire decoded slab.
/// This does not bound the pre-existing WAL/index owner or process RSS.
pub(super) fn collect(
    artifacts: &[SemanticIndexArtifact],
    embedding: &[f32],
    limit: usize,
    filter: Option<&dyn FsSearchFilter>,
) -> Result<Vec<VectorSearchResult>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    ensure!(embedding.iter().all(|value| value.is_finite()),
        "exact semantic query must be finite");
    // Validate the complete cohort before any filter invocation or scoring.
    for artifact in artifacts {
        ensure!(artifact.index().dimension() == embedding.len(),
            "exact semantic query dimension mismatch");
    }
    let mut selection = Selection::new(limit);
    let mut scorer = RowScorer::new();
    let mut main_scored = 0usize;
    let mut wal_scored = 0usize;
    for artifact in artifacts {
        let index = artifact.index();
        let shadowed = ShadowIds::new(index)?;
        // Physical row order preserves the incumbent's best-chunk tie break.
        // A replacement shadows EVERY duplicate main row, even if the current
        // WAL value is excluded by the original query's scope.
        for row in 0..index.record_count() {
            if index.is_deleted(row) {
                continue;
            }
            let doc_id = index.doc_id_at(row)?;
            if shadowed.contains(doc_id)
                || filter.is_some_and(|filter| !filter.matches(doc_id, None)) {
                continue;
            }
            let identity = parse_semantic_doc_id(doc_id)
                .context("retained semantic main row has an invalid message identity")?;
            let score = scorer.score(index, row, embedding)?;
            selection.consider(Winner {
                message_id: identity.message_id, chunk_idx: identity.chunk_idx, score,
            })?;
            main_scored = main_scored.saturating_add(1);
        }
        for (doc_id, vector) in index.wal_records() {
            if filter.is_some_and(|filter| !filter.matches(doc_id, None)) {
                continue;
            }
            let identity = parse_semantic_doc_id(doc_id)
                .context("retained semantic WAL row has an invalid message identity")?;
            selection.consider(Winner {
                message_id: identity.message_id,
                chunk_idx: identity.chunk_idx,
                score: dot_product_f32_f32(vector, embedding)?,
            })?;
            wal_scored = wal_scored.saturating_add(1);
        }
    }
    let hits: Vec<_> = selection.finish().into_iter().map(|hit| VectorSearchResult {
        message_id: hit.message_id, chunk_idx: hit.chunk_idx, score: hit.score,
    }).collect();
    tracing::debug!(shard_count = artifacts.len(), main_scored, wal_scored,
        returned = hits.len(), "complete retained-WAL exact message selection");
    Ok(hits)
}
