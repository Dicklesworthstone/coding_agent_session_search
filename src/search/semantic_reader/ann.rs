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
    NativeHnswGenerationReceiptV2, ValidatedNativeHnsw, native_hnsw_generation_receipt_path,
};
use frankensearch::index::{ValidatedFsviBytes, dot_product_f32_f32};
use serde::Serialize;

use crate::search::semantic_manifest::{
    SemanticAnnBaseBindingV1, SemanticArtifactRole, SemanticGenerationArtifact,
    SemanticGenerationManifestV1,
};

use super::{SemanticGenerationReader, SemanticReaderError, SemanticReaderResult, TierKind};

/// A graph and the complete receipt selected by the publication layer. Merely
/// finding a graph beside an FSVI is not sufficient to populate this value.
#[derive(Debug, Clone)]
pub struct SemanticAnnExpectation {
    pub graph_path: PathBuf,
    pub receipt: NativeHnswGenerationReceiptV2,
}

/// Native graph format with its complete v2 receipt sealed into the manifest.
/// `artifact_parameters_sha256` is the canonical receipt fingerprint, not a
/// digest invented from a subset of HNSW parameters or inferred from a filename.
pub const NATIVE_ANN_ARTIFACT_FORMAT: &str = "native-hnsw-v2";

/// Preflight cap across all manifest-selected graph images. The loaded graph,
/// parser scratch, already-retained vectors and concurrent file growth are NOT
/// covered by this declared-byte limit. Zero explicitly disables graph loading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnnAdmissionBudget {
    pub max_declared_graph_bytes: u64,
}

impl Default for AnnAdmissionBudget {
    fn default() -> Self {
        Self { max_declared_graph_bytes: 256 * 1024 * 1024 }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum NativeAnnArtifactError {
    #[error("native ANN requires an exact FSVI v2 base artifact")]
    InvalidBase,
    #[error("native ANN receipt does not bind the selected vector artifact")]
    ReceiptMismatch,
    #[error("native ANN requires a safe relative path with the receipted graph basename")]
    InvalidPath,
}

/// Describe a graph produced by `ValidatedNativeHnsw::save` for sealing in the
/// SAME immutable generation as `base`. This does not write or publish anything.
/// The manifest sealer still validates the actual graph bytes before publication.
/// Preserve the adjacent binary receipt: serving verifies its entire fingerprint.
pub fn native_ann_artifact(
    base: &SemanticGenerationArtifact,
    receipt: &NativeHnswGenerationReceiptV2,
    relative_path: &str,
    validated_at_ms: i64,
) -> Result<SemanticGenerationArtifact, NativeAnnArtifactError> {
    let role = match base.role {
        SemanticArtifactRole::FastVector => SemanticArtifactRole::FastAnn,
        SemanticArtifactRole::QualityVector => SemanticArtifactRole::QualityAnn,
        _ => return Err(NativeAnnArtifactError::InvalidBase),
    };
    if base.artifact_format != "fsvi-v2" || base.ann_base.is_some() || base.wal_entry_count != 0 {
        return Err(NativeAnnArtifactError::InvalidBase);
    }
    if receipt.validate().is_err() || !receipt_matches_base(receipt, base) {
        return Err(NativeAnnArtifactError::ReceiptMismatch);
    }
    let path = Path::new(relative_path);
    if !crate::search::semantic_manifest::semantic_shard_artifact_path_is_safe(relative_path)
        || relative_path.contains('\\')
        || relative_path.contains(':')
        || path.file_name().and_then(|name| name.to_str()) != Some(receipt.graph_basename.as_str())
        || native_hnsw_generation_receipt_path(path).is_err()
    {
        return Err(NativeAnnArtifactError::InvalidPath);
    }
    let mut artifact = base.clone();
    artifact.role = role;
    artifact.relative_path = relative_path.to_owned();
    artifact.artifact_format = NATIVE_ANN_ARTIFACT_FORMAT.to_owned();
    artifact.artifact_sha256 = receipt.graph_sha256.clone();
    artifact.size_bytes = receipt.graph_byte_len;
    artifact.artifact_parameters_sha256 = receipt.receipt_sha256.clone();
    artifact.ann_base = Some(SemanticAnnBaseBindingV1 {
        base_role: base.role,
        base_artifact_sha256: base.artifact_sha256.clone(),
        base_storage_fingerprint: receipt.vector_storage_fingerprint.clone(),
        base_artifact_parameters_sha256: base.artifact_parameters_sha256.clone(),
        algorithm: "native-hnsw".to_owned(),
        metric: "cosine".to_owned(),
        parameters_sha256: receipt.receipt_sha256.clone(),
    });
    artifact.validation.validator_id = "cass-native-hnsw-v2".to_owned();
    artifact.validation.validated_at_ms = validated_at_ms;
    artifact.validation.artifact_sha256 = artifact.artifact_sha256.clone();
    artifact.validation.size_bytes = artifact.size_bytes;
    Ok(artifact)
}

fn receipt_matches_base(
    receipt: &NativeHnswGenerationReceiptV2,
    base: &SemanticGenerationArtifact,
) -> bool {
    receipt.fsvi_whole_image_sha256 == base.artifact_sha256
        && receipt.embedding_identity_fingerprint == base.embedding_identity.fingerprint
        && receipt.vector_storage_fingerprint == base.embedding_identity.identity.storage.fingerprint()
        && receipt.fsvi_physical_row_count == base.vector_slot_count
        && receipt.point_count == base.vector_slot_count
        && receipt.ordered_live_docset_digest == base.covered_live_docset_sha256
}

fn graph_bytes_within_budget(mut sizes: impl Iterator<Item = u64>, budget: AnnAdmissionBudget) -> bool {
    sizes.try_fold(0_u64, |total, bytes| total.checked_add(bytes))
        .is_some_and(|total| total <= budget.max_declared_graph_bytes && usize::try_from(total).is_ok())
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
    AdmissionBudget,
    CandidateLimit,
    FilterUnderfill,
    QueryFailed,
}

/// Admission is not query execution. A loaded graph may still be bypassed by
/// an exact request, a zero-k request, or a candidate-limit fallback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SemanticAnnAdmission {
    Unavailable {
        reason: AnnFallbackReason,
    },
    Admitted {
        graph_sha256: String,
        receipt_sha256: String,
    },
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
        Self {
            initial_candidates: 128,
            max_candidates: 4096,
        }
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
pub enum SemanticShardEngine {
    Skipped,
    Exact,
    NativeAnn,
    ExactFallback,
}

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
            || expected.receipt.fsvi_whole_image_sha256
                != hex::encode(owner.witness().whole_image_sha256)
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

    fn load_manifest(
        owner: Arc<ValidatedFsviBytes>,
        artifact: &SemanticGenerationArtifact,
        base: &SemanticGenerationArtifact,
        generation_dir: &Path,
    ) -> Self {
        let Some(binding) = artifact.ann_base.as_ref() else {
            return Self::Unavailable(AnnFallbackReason::InvalidExpectation);
        };
        // Validate the selected manifest against the RETAINED owner, not an
        // FSVI reopened at the publication path. A replaced path is irrelevant.
        if artifact.artifact_format != NATIVE_ANN_ARTIFACT_FORMAT
            || binding.algorithm != "native-hnsw" || binding.metric != "cosine"
            || hex::encode(owner.witness().whole_image_sha256) != base.artifact_sha256
            || hex::encode(owner.identity_v2().identity_bundle_fingerprint) != base.embedding_identity.fingerprint
        {
            return Self::Unavailable(AnnFallbackReason::InvalidExpectation);
        }
        let graph_path = generation_dir.join(&artifact.relative_path);
        let Ok(receipt_path) = native_hnsw_generation_receipt_path(&graph_path) else {
            return Self::Unavailable(AnnFallbackReason::UnsafePath);
        };
        if !single_link_regular(&graph_path) || !single_link_regular(&receipt_path) {
            return Self::Unavailable(AnnFallbackReason::SidecarUnavailable);
        }
        // Fixed-format receipts are small. Do not feed an obviously oversized
        // sidecar to the upstream read-all loader. This is a preflight, not a
        // race-free allocation ceiling; upstream byte/identity checks still run.
        let graph_size = std::fs::metadata(&graph_path).ok().map(|meta| meta.len());
        let receipt_size = std::fs::metadata(&receipt_path).ok().map(|meta| meta.len());
        if graph_size != Some(artifact.size_bytes) {
            return Self::Unavailable(AnnFallbackReason::ReceiptMismatch);
        }
        if receipt_size.is_none_or(|bytes| bytes > 64 * 1024) {
            return Self::Unavailable(AnnFallbackReason::AdmissionBudget);
        }
        // The upstream loader performs structural and cryptographic admission
        // with this exact owner. Keep that loaded graph: no second graph open.
        match ValidatedNativeHnsw::load(owner, &graph_path) {
            Ok((graph, receipt))
                if receipt.graph_sha256 == artifact.artifact_sha256
                    && receipt.graph_byte_len == artifact.size_bytes
                    && receipt.receipt_sha256 == artifact.artifact_parameters_sha256
                    && receipt_matches_base(&receipt, base) =>
            {
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
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return false;
        }
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
        match kind {
            TierKind::Fast => self.fast.get(shard),
            TierKind::Quality => self.quality.get(shard),
        }
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
                let tier = self
                    .tier(kind)
                    .ok_or(SemanticReaderError::MissingTier(kind))?;
                if selected.len() != tier.shards.len() {
                    return Err(SemanticReaderError::AnnSelectionMismatch(kind));
                }
            }
        }
        let load_tier = |kind, selected: Option<&[Option<SemanticAnnExpectation>]>| {
            self.tier(kind).map_or_else(Vec::new, |tier| {
                tier.shards
                    .iter()
                    .enumerate()
                    .map(|(position, owner)| {
                        ShardAnn::load(
                            Arc::clone(owner),
                            selected.and_then(|list| list[position].as_ref()),
                        )
                    })
                    .collect()
            })
        };
        let selected = AnnSelection {
            fast: load_tier(TierKind::Fast, fast),
            quality: load_tier(TierKind::Quality, quality),
        };
        self.ann = Arc::new(selected);
        Ok(self)
    }

    /// Load only ANN roles explicitly declared by a complete v1 manifest.
    /// The caller supplies selection authority; use SelectedSemanticGeneration
    /// for current.json-selected serving. This API never discovers adjacent
    /// graphs, reopens vector paths, or treats graph failure as loss of a tier.
    /// A v1 manifest names one vector per tier; sharded selections fail back to
    /// exact rather than applying one graph to unrelated shards.
    pub fn with_manifest_ann(
        mut self,
        manifest: &SemanticGenerationManifestV1,
        generation_dir: &Path,
        budget: AnnAdmissionBudget,
    ) -> Self {
        let manifest_valid = manifest.validate().is_ok();
        let within_budget = graph_bytes_within_budget(
            manifest.artifacts.iter().filter(|artifact| matches!(artifact.role,
                SemanticArtifactRole::FastAnn | SemanticArtifactRole::QualityAnn))
                .map(|artifact| artifact.size_bytes),
            budget,
        );
        let load = |kind, ann_role, base_role| {
            self.tier(kind).map_or_else(Vec::new, |tier| {
                tier.shards.iter().map(|owner| {
                    let Some(artifact) = manifest.artifact(ann_role) else {
                        return ShardAnn::Unavailable(AnnFallbackReason::NotSelected);
                    };
                    if !manifest_valid || tier.shards.len() != 1 {
                        return ShardAnn::Unavailable(AnnFallbackReason::InvalidExpectation);
                    }
                    if !within_budget {
                        return ShardAnn::Unavailable(AnnFallbackReason::AdmissionBudget);
                    }
                    let Some(base) = manifest.artifact(base_role) else {
                        return ShardAnn::Unavailable(AnnFallbackReason::InvalidExpectation);
                    };
                    ShardAnn::load_manifest(Arc::clone(owner), artifact, base, generation_dir)
                }).collect()
            })
        };
        self.ann = Arc::new(AnnSelection {
            fast: load(TierKind::Fast, SemanticArtifactRole::FastAnn, SemanticArtifactRole::FastVector),
            quality: load(TierKind::Quality, SemanticArtifactRole::QualityAnn, SemanticArtifactRole::QualityVector),
        });
        self
    }

    /// Missing tier/shard is None, distinct from an exact shard without ANN.
    pub fn ann_admission(&self, tier: TierKind, shard: usize) -> Option<SemanticAnnAdmission> {
        self.tier(tier)?.shards.get(shard)?;
        Some(self.ann.shard(tier, shard).map_or(
            SemanticAnnAdmission::Unavailable {
                reason: AnnFallbackReason::NotSelected,
            },
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
        Some(ShardAnn::Unavailable(reason)) => {
            report.fallback_reason = Some(*reason);
            None
        }
        None => {
            report.fallback_reason = Some(AnnFallbackReason::NotSelected);
            None
        }
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
    if candidates.len() > width {
        return Err(SemanticReaderError::InvalidCandidate);
    }
    let count = candidates.len();
    let mut hits = Vec::with_capacity(count);
    let mut seen = HashSet::with_capacity(count);
    for candidate in candidates {
        let row = usize::try_from(candidate.physical_row())
            .map_err(|_| SemanticReaderError::InvalidCandidate)?;
        if !seen.insert(row)
            || !candidate.flags().is_live()
            || owner.doc_id_at(row)? != candidate.doc_id()
        {
            return Err(SemanticReaderError::InvalidCandidate);
        }
        if filter.is_some_and(|filter| !filter.matches(candidate.doc_id(), None)) {
            continue;
        }
        // 1.0 - distance loses small scores to rounding. Recompute from the
        // exact retained source at its declared storage precision instead.
        let score = dot_product_f32_f32(&owner.vector_at_f32(row)?, query.vector())?;
        if !score.is_finite() {
            return Err(SemanticReaderError::InvalidCandidate);
        }
        hits.push(VectorHit {
            index: candidate.physical_row(),
            score,
            doc_id: candidate.doc_id().into(),
        });
    }
    hits.sort_unstable_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.index.cmp(&right.index))
    });
    Ok((hits, count))
}

#[cfg(test)]
mod publication_budget_tests {
    use super::*;

    #[test]
    fn combined_graph_budget_is_checked_before_loading_either_tier() {
        let budget = AnnAdmissionBudget { max_declared_graph_bytes: 100 };
        assert!(graph_bytes_within_budget([40, 60].into_iter(), budget));
        assert!(!graph_bytes_within_budget([40, 61].into_iter(), budget));
    }

    #[test]
    fn overflow_is_not_misreported_as_an_empty_graph_selection() {
        let budget = AnnAdmissionBudget { max_declared_graph_bytes: u64::MAX };
        assert!(!graph_bytes_within_budget([u64::MAX, 1].into_iter(), budget));
    }

    #[test]
    fn zero_budget_disables_selected_graphs_but_not_an_absent_selection() {
        let budget = AnnAdmissionBudget { max_declared_graph_bytes: 0 };
        assert!(graph_bytes_within_budget(std::iter::empty(), budget));
        assert!(!graph_bytes_within_budget([1].into_iter(), budget));
    }

    #[test]
    fn graph_budget_rejection_has_a_stable_machine_readable_reason() {
        let admission = SemanticAnnAdmission::Unavailable { reason: AnnFallbackReason::AdmissionBudget };
        assert_eq!(serde_json::to_value(admission).unwrap(),
            serde_json::json!({"state": "unavailable", "reason": "admission_budget"}));
    }
}
