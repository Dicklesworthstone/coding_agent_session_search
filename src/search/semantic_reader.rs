//! Semantic retrieval over retained, witness-checked FSVI v2 shards.
//! Exact is the default; opt-in native ANN has per-shard exact fallback.
//!
//! This is the reader component of the immutable-generation migration. Callers
//! must supply the complete artifact selection, bindings and witnesses from a
//! separately validated publication. It does NOT infer them from a file name,
//! model name, dimension, legacy manifest, or the bytes being reopened. Nor does
//! it establish that the selected artifacts cover the current canonical DB.
//!
//! Every shard is reopened against its entire expected witness. Its sealed byte
//! owner stays inside the reader and every returned result batch. Search never
//! reopens a pathname, exposes a mutable VectorIndex, or silently substitutes a
//! fast tier for a requested quality tier. The existing CLI publication and
//! source-bound hydration gates are deliberately not bypassed by this API.
//!
//! Owners retain complete artifact images in memory. Query merging retains at
//! most O(k) candidates per active tier; this is NOT a bound on total index RSS.

pub mod ann;
pub mod publication;

use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use frankensearch::core::filter::SearchFilter;
use frankensearch::core::generation::ArtifactGenerationIdentityV1;
use frankensearch::core::{
    BoundQueryEmbedding, RetrievalTopology, SearchError, SpaceIdentityAdmission,
    TieredQueryEmbeddings,
};
use frankensearch::index::{
    FsviAdmissionError, FsviV2IdentityBinding, FsviV2Witness, ValidatedFsviBytes,
};

use super::semantic_manifest::TierKind;
use super::vector_index::{
    ROLE_ASSISTANT, ROLE_SYSTEM, ROLE_TOOL, ROLE_USER, SemanticDocId, parse_semantic_doc_id,
};
use ann::{AnnSearchPolicy, SemanticShardEngine, SemanticShardExecution};

/// A publication's expected artifact, not a claim derived while opening it.
#[derive(Debug, Clone)]
pub struct SemanticShardExpectation {
    pub path: PathBuf,
    pub binding: FsviV2IdentityBinding,
    pub witness: FsviV2Witness,
}

/// Admission and retrieval failures retain the upstream typed cause.
#[derive(Debug, thiserror::Error)]
pub enum SemanticReaderError {
    #[error("semantic reader requires at least one selected tier")]
    NoTiers,
    #[error("a selected semantic tier must name at least one shard")]
    EmptyTier,
    #[error("selected semantic artifacts do not belong to one full-width generation")]
    MixedGeneration,
    #[error("shards within one semantic tier have different complete identity bindings")]
    MixedTierIdentity,
    #[error("fast and quality roles refer to the same witnessed artifact image")]
    ArtifactRoleAlias,
    #[error("semantic artifact admission failed for {tier:?} shard {shard}: {source}")]
    Admission {
        tier: TierKind,
        shard: usize,
        #[source]
        source: Box<FsviAdmissionError>,
    },
    #[error("ANN selection does not align with the {0:?} tier's shard order")]
    AnnSelectionMismatch(TierKind),
    #[error("ANN candidate limits must satisfy 1 <= initial <= maximum <= 65536")]
    InvalidAnnPolicy,
    #[error("a semantic shard must be a WAL-free admitted image with canonical CASS passage IDs")]
    NonCanonicalDocuments,
    #[error("a canonical passage ID occurs more than once within a semantic tier")]
    DuplicateDocument,
    #[error("semantic live-document count exceeds u64")]
    CountOverflow,
    #[error("the query requested an unavailable {0:?} semantic tier")]
    MissingTier(TierKind),
    #[error("the query producer is not identical to the admitted {0:?} producer")]
    ForeignProducer(TierKind),
    #[error("progressive retrieval requires both fast and quality query bindings")]
    ProgressiveRequiresBothTiers,
    #[error("replacement must advance the generation or exactly match the retained selection")]
    StaleReplacement,
    #[error("the vector engine returned an inconsistent candidate identity or score")]
    InvalidCandidate,
    #[error(transparent)]
    Search(#[from] SearchError),
}

pub type SemanticReaderResult<T> = Result<T, SemanticReaderError>;

#[derive(Debug)]
struct AdmittedTier {
    binding: FsviV2IdentityBinding,
    shards: Vec<Arc<ValidatedFsviBytes>>,
    live_count: u64,
}

impl AdmittedTier {
    fn open(kind: TierKind, selected: &[SemanticShardExpectation]) -> SemanticReaderResult<Self> {
        let first = selected.first().ok_or(SemanticReaderError::EmptyTier)?;
        for shard in selected {
            if shard.binding.generation() != first.binding.generation()
                || shard.witness.generation != first.binding.generation()
            {
                return Err(SemanticReaderError::MixedGeneration);
            }
            if shard.binding != first.binding {
                return Err(SemanticReaderError::MixedTierIdentity);
            }
        }
        let mut shards = Vec::with_capacity(selected.len());
        let mut live_count = 0_u64;
        for (position, expected) in selected.iter().enumerate() {
            let owner = ValidatedFsviBytes::reopen_exact(
                &expected.path,
                &expected.binding,
                &expected.witness,
            )
            .map_err(|source| SemanticReaderError::Admission {
                tier: kind,
                shard: position,
                source: Box::new(source),
            })?;
            if !owner.published_wal_absent() {
                return Err(SemanticReaderError::NonCanonicalDocuments);
            }
            live_count = live_count
                .checked_add(owner.witness().live_count)
                .ok_or(SemanticReaderError::CountOverflow)?;
            shards.push(Arc::new(owner));
        }

        // One temporary set of borrowed IDs, not a second copy of the corpus.
        // Checking the entire tier before exposing it makes shard-local top-k
        // sufficient for an exact global passage top-k without dedup underfill.
        let mut ids = HashSet::new();
        for shard in &shards {
            for position in 0..shard.record_count() {
                let id = shard.doc_id_at(position)?;
                canonical_document(id)?;
                if shard.row(position)?.flags().is_live() && !ids.insert(id) {
                    return Err(SemanticReaderError::DuplicateDocument);
                }
            }
        }
        drop(ids);
        Ok(Self {
            binding: first.binding.clone(),
            shards,
            live_count,
        })
    }

    fn activate(&self, query: &BoundQueryEmbedding, kind: TierKind) -> SemanticReaderResult<()> {
        let admission = query
            .verify_producer_conformance(&self.binding.frozen_identity().identity, kind.as_str())?;
        if !matches!(admission, SpaceIdentityAdmission::SameProducer) {
            // A certified-compatible foreign producer is comparison telemetry,
            // never permission to serve its vectors against this artifact.
            return Err(SemanticReaderError::ForeignProducer(kind));
        }
        if query.vector().len() != self.binding.dimension() {
            return Err(SearchError::DimensionMismatch {
                expected: self.binding.dimension(),
                found: query.vector().len(),
            }
            .into());
        }
        Ok(())
    }

    fn same_selection(&self, other: &Self) -> bool {
        self.binding == other.binding
            && self.shards.len() == other.shards.len()
            && self
                .shards
                .iter()
                .zip(&other.shards)
                .all(|(left, right)| left.witness() == right.witness())
    }

    fn search(
        &self,
        kind: TierKind,
        query: &BoundQueryEmbedding,
        k: usize,
        filter: Option<&dyn SearchFilter>,
        ann: &ann::AnnSelection,
        policy: Option<AnnSearchPolicy>,
    ) -> SemanticReaderResult<TierResult> {
        let mut execution = Vec::with_capacity(self.shards.len());
        // Do not allocate from caller k: allocation grows only with real hits.
        let mut best = BinaryHeap::<Candidate>::new();
        for (shard, owner) in self.shards.iter().enumerate() {
            let local_limit = k.min(owner.live_count());
            // k is a requested maximum, never an allocation hint larger than
            // the actual shard. Preserve identity checks even for empty shards.
            let report = SemanticShardExecution {
                tier: kind,
                shard,
                engine: SemanticShardEngine::Skipped,
                fallback_reason: None,
                graph_sha256: None,
                ann_windows: 0,
                candidate_rows: 0,
                final_candidate_limit: 0,
                returned_candidates: 0,
            };
            let (local, report) = ann::search_shard(
                owner,
                ann.shard(kind, shard),
                query,
                local_limit,
                filter,
                policy,
                report,
            )?;
            execution.push(report);
            if local.len() > local_limit {
                return Err(SemanticReaderError::InvalidCandidate);
            }
            for (source_rank, hit) in local.into_iter().enumerate() {
                let physical_index = usize::try_from(hit.index)
                    .map_err(|_| SemanticReaderError::InvalidCandidate)?;
                if !hit.score.is_finite()
                    || !owner.row(physical_index)?.flags().is_live()
                    || owner.doc_id_at(physical_index)? != hit.doc_id.as_str()
                {
                    return Err(SemanticReaderError::InvalidCandidate);
                }
                let candidate = Candidate {
                    document: canonical_document(hit.doc_id.as_str())?,
                    doc_id: hit.doc_id.to_string(),
                    score: hit.score,
                    shard,
                    physical_index,
                    source_rank,
                };
                if best.len() < k {
                    best.push(candidate);
                } else if let Some(mut worst) = best.peek_mut()
                    && candidate < *worst
                {
                    *worst = candidate;
                }
            }
        }
        Ok(TierResult {
            candidates: best.into_sorted_vec(),
            execution,
        })
    }
}

#[derive(Debug, Clone)]
struct TierResult {
    candidates: Vec<Candidate>,
    execution: Vec<SemanticShardExecution>,
}

fn canonical_document(id: &str) -> SemanticReaderResult<SemanticDocId> {
    let parsed = parse_semantic_doc_id(id).ok_or(SemanticReaderError::NonCanonicalDocuments)?;
    if parsed.content_hash.is_none()
        || parsed.message_id > i64::MAX as u64
        || !matches!(
            parsed.role,
            ROLE_USER | ROLE_ASSISTANT | ROLE_SYSTEM | ROLE_TOOL
        )
        || parsed.to_doc_id_string() != id
    {
        return Err(SemanticReaderError::NonCanonicalDocuments);
    }
    Ok(parsed)
}

/// A complete admitted selection. Clones share sealed images, not pathnames.
///
/// Quality-only selections are first-class; no fast artifact is required.
#[derive(Debug, Clone)]
pub struct SemanticGenerationReader {
    fast: Option<Arc<AdmittedTier>>,
    quality: Option<Arc<AdmittedTier>>,
    generation: ArtifactGenerationIdentityV1,
    ann: Arc<ann::AnnSelection>,
}

impl SemanticGenerationReader {
    /// Admit every explicitly selected shard. No legacy fallback or discovery.
    ///
    /// This reader does not prove that the caller supplied every shard required
    /// by current.json. The publication selector must establish that separately.
    pub fn open(
        fast: Option<&[SemanticShardExpectation]>,
        quality: Option<&[SemanticShardExpectation]>,
    ) -> SemanticReaderResult<Self> {
        let first = fast
            .or(quality)
            .ok_or(SemanticReaderError::NoTiers)?
            .first()
            .ok_or(SemanticReaderError::EmptyTier)?;
        let generation = first.binding.generation();
        // Refuse a mixed pair before opening any artifact. Compare the nonce
        // as well as the sequence: equal low-width counters prove nothing.
        for selected in [fast, quality].into_iter().flatten() {
            if selected.is_empty() {
                return Err(SemanticReaderError::EmptyTier);
            }
            if selected
                .iter()
                .any(|shard| shard.binding.generation() != generation)
            {
                return Err(SemanticReaderError::MixedGeneration);
            }
        }
        let fast = fast
            .map(|selected| AdmittedTier::open(TierKind::Fast, selected))
            .transpose()?
            .map(Arc::new);
        let quality = quality
            .map(|selected| AdmittedTier::open(TierKind::Quality, selected))
            .transpose()?
            .map(Arc::new);
        if let (Some(fast), Some(quality)) = (&fast, &quality) {
            let fast_images: HashSet<_> = fast
                .shards
                .iter()
                .map(|owner| owner.witness().whole_image_sha256)
                .collect();
            if quality
                .shards
                .iter()
                .any(|owner| fast_images.contains(&owner.witness().whole_image_sha256))
            {
                return Err(SemanticReaderError::ArtifactRoleAlias);
            }
        }
        Ok(Self {
            fast,
            quality,
            generation,
            ann: Arc::new(ann::AnnSelection::default()),
        })
    }

    /// Install only a completely admitted successor. Previously returned batches
    /// and cloned readers continue to own the old images after a successful swap.
    pub fn try_replace(
        &mut self,
        fast: Option<&[SemanticShardExpectation]>,
        quality: Option<&[SemanticShardExpectation]>,
    ) -> SemanticReaderResult<()> {
        let candidate = Self::open(fast, quality)?;
        let same_tier = |left: &Option<Arc<AdmittedTier>>, right: &Option<Arc<AdmittedTier>>| match (
            left, right,
        ) {
            (None, None) => true,
            (Some(left), Some(right)) => left.same_selection(right),
            _ => false,
        };
        if candidate.generation.sequence <= self.generation.sequence
            && !(same_tier(&self.fast, &candidate.fast)
                && same_tier(&self.quality, &candidate.quality))
        {
            return Err(SemanticReaderError::StaleReplacement);
        }
        *self = candidate;
        Ok(())
    }

    pub fn generation(&self) -> ArtifactGenerationIdentityV1 {
        self.generation
    }

    /// Read an exact retained witness without reopening its publication path.
    pub fn witness(&self, tier: TierKind, shard: usize) -> Option<&FsviV2Witness> {
        self.tier(tier)?
            .shards
            .get(shard)
            .map(|owner| owner.witness())
    }

    fn tier(&self, kind: TierKind) -> Option<&AdmittedTier> {
        match kind {
            TierKind::Fast => self.fast.as_deref(),
            TierKind::Quality => self.quality.as_deref(),
        }
    }

    /// Complete every requested producer/space join before making search callable.
    /// This also validates zero-k and empty-corpus requests; neither bypasses a
    /// foreign-space refusal. Hash-control bindings remain HashControl topology.
    pub fn activate<'reader, 'query>(
        &'reader self,
        queries: &'query TieredQueryEmbeddings,
    ) -> SemanticReaderResult<ActivatedSemanticSearch<'reader, 'query>> {
        for (kind, query) in [
            (TierKind::Fast, queries.fast()),
            (TierKind::Quality, queries.quality()),
        ] {
            if let Some(query) = query {
                self.tier(kind)
                    .ok_or(SemanticReaderError::MissingTier(kind))?
                    .activate(query, kind)?;
            }
        }
        if queries.fast().is_none() && queries.quality().is_none() {
            return Err(SemanticReaderError::NoTiers);
        }
        Ok(ActivatedSemanticSearch {
            reader: self,
            queries,
        })
    }
}

/// Search capability bound to both immutable artifacts and typed query vectors.
#[derive(Debug)]
pub struct ActivatedSemanticSearch<'reader, 'query> {
    reader: &'reader SemanticGenerationReader,
    queries: &'query TieredQueryEmbeddings,
}

impl<'reader, 'query> ActivatedSemanticSearch<'reader, 'query> {
    /// Search each requested tier independently, then merge passage identities.
    /// Single-tier requests retain exact scores/order. Two-tier requests use
    /// rank fusion, never compare raw cosine scores from unrelated model spaces.
    pub fn search(
        &self,
        k: usize,
        filter: Option<&dyn SearchFilter>,
    ) -> SemanticReaderResult<SemanticSearchBatch> {
        self.search_impl(k, filter, None)
    }

    /// Opt into per-shard native ANN. A missing/rejected graph, exhausted
    /// candidate budget, or graph-query failure uses that shard's retained
    /// exact owner. Inspect execution() on the batch for what actually ran.
    /// Narrow ANN windows are approximate; no recall guarantee is implied.
    pub fn search_with_ann(
        &self,
        k: usize,
        filter: Option<&dyn SearchFilter>,
        policy: AnnSearchPolicy,
    ) -> SemanticReaderResult<SemanticSearchBatch> {
        policy.validate()?;
        self.search_impl(k, filter, Some(policy))
    }

    fn search_impl(
        &self,
        k: usize,
        filter: Option<&dyn SearchFilter>,
        policy: Option<AnnSearchPolicy>,
    ) -> SemanticReaderResult<SemanticSearchBatch> {
        let fast = self.search_tier(TierKind::Fast, k, filter, policy)?;
        let quality = self.search_tier(TierKind::Quality, k, filter, policy)?;
        Ok(make_batch(
            self.reader,
            self.queries.supported_topology(),
            SemanticResultPhase::Complete,
            fast,
            quality,
            k,
        ))
    }

    /// Yield fast results before scanning quality vectors. On the next call,
    /// retrieve independent quality candidates and rank-fuse the two lists.
    /// Dropping the iterator after Initial performs no quality search. Keep
    /// filter semantics stable across calls (SearchFilter permits interior
    /// mutability); the reader and query bindings themselves cannot change.
    pub fn progressive<'call>(
        &'call self,
        k: usize,
        filter: Option<&'call dyn SearchFilter>,
    ) -> SemanticReaderResult<ProgressiveSemanticSearch<'call, 'reader, 'query>> {
        self.progressive_impl(k, filter, None)
    }

    /// Same lazy phase ordering as progressive(), with optional native ANN on
    /// each tier. Dropping Initial never starts a quality graph/vector search.
    pub fn progressive_with_ann<'call>(
        &'call self,
        k: usize,
        filter: Option<&'call dyn SearchFilter>,
        policy: AnnSearchPolicy,
    ) -> SemanticReaderResult<ProgressiveSemanticSearch<'call, 'reader, 'query>> {
        policy.validate()?;
        self.progressive_impl(k, filter, Some(policy))
    }

    fn progressive_impl<'call>(
        &'call self,
        k: usize,
        filter: Option<&'call dyn SearchFilter>,
        policy: Option<AnnSearchPolicy>,
    ) -> SemanticReaderResult<ProgressiveSemanticSearch<'call, 'reader, 'query>> {
        if self.queries.fast().is_none() || self.queries.quality().is_none() {
            return Err(SemanticReaderError::ProgressiveRequiresBothTiers);
        }
        Ok(ProgressiveSemanticSearch {
            search: self,
            k,
            filter,
            policy,
            state: ProgressiveState::Initial,
        })
    }

    fn search_tier(
        &self,
        kind: TierKind,
        k: usize,
        filter: Option<&dyn SearchFilter>,
        policy: Option<AnnSearchPolicy>,
    ) -> SemanticReaderResult<Option<TierResult>> {
        let query = match kind {
            TierKind::Fast => self.queries.fast(),
            TierKind::Quality => self.queries.quality(),
        };
        query
            .map(|query| {
                self.reader
                    .tier(kind)
                    .ok_or(SemanticReaderError::MissingTier(kind))?
                    .search(kind, query, k, filter, &self.reader.ann, policy)
            })
            .transpose()
    }
}

enum ProgressiveState {
    Initial,
    Refine(TierResult),
    Done,
}

/// A lazy two-phase retrieval. An error terminates the iterator; it never
/// relabels the initial fast list as a successfully refined complete result.
pub struct ProgressiveSemanticSearch<'call, 'reader, 'query> {
    search: &'call ActivatedSemanticSearch<'reader, 'query>,
    k: usize,
    filter: Option<&'call dyn SearchFilter>,
    state: ProgressiveState,
    policy: Option<AnnSearchPolicy>,
}

impl std::fmt::Debug for ProgressiveSemanticSearch<'_, '_, '_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let phase = match &self.state {
            ProgressiveState::Initial => "initial",
            ProgressiveState::Refine(_) => "refine",
            ProgressiveState::Done => "done",
        };
        formatter
            .debug_struct("ProgressiveSemanticSearch")
            .field("limit", &self.k)
            .field("next_phase", &phase)
            .finish_non_exhaustive()
    }
}

impl Iterator for ProgressiveSemanticSearch<'_, '_, '_> {
    type Item = SemanticReaderResult<SemanticSearchBatch>;

    fn next(&mut self) -> Option<Self::Item> {
        let state = std::mem::replace(&mut self.state, ProgressiveState::Done);
        match state {
            ProgressiveState::Initial => {
                let result = self
                    .search
                    .search_tier(TierKind::Fast, self.k, self.filter, self.policy)
                    .and_then(|hits| hits.ok_or(SemanticReaderError::MissingTier(TierKind::Fast)));
                Some(result.map(|fast| {
                    let batch = make_batch(
                        self.search.reader,
                        self.search.queries.supported_topology(),
                        SemanticResultPhase::Initial,
                        Some(fast.clone()),
                        None,
                        self.k,
                    );
                    self.state = ProgressiveState::Refine(fast);
                    batch
                }))
            }
            ProgressiveState::Refine(fast) => {
                let result = self
                    .search
                    .search_tier(TierKind::Quality, self.k, self.filter, self.policy)
                    .and_then(|hits| {
                        hits.ok_or(SemanticReaderError::MissingTier(TierKind::Quality))
                    });
                Some(result.map(|quality| {
                    make_batch(
                        self.search.reader,
                        self.search.queries.supported_topology(),
                        SemanticResultPhase::Refined,
                        Some(fast),
                        Some(quality),
                        self.k,
                    )
                }))
            }
            ProgressiveState::Done => None,
        }
    }
}

impl std::iter::FusedIterator for ProgressiveSemanticSearch<'_, '_, '_> {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticResultPhase {
    Initial,
    Refined,
    Complete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticScoreKind {
    Exact,
    /// Exact-source dot products over ANN-selected candidates, NOT exact top-k.
    AnnRescored,
    ReciprocalRankFusion,
}

/// A source location always includes its shard. It is never a global row index.
#[derive(Debug, Clone, PartialEq)]
pub struct SemanticHitSource {
    pub shard: usize,
    pub physical_index: usize,
    /// One-based rank within this tier's globally merged, filtered candidate list.
    pub tier_rank: usize,
    pub score: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SemanticPassageHit {
    pub doc_id: String,
    pub document: SemanticDocId,
    pub ranking_score: f64,
    pub fast: Option<SemanticHitSource>,
    pub quality: Option<SemanticHitSource>,
}

/// Counts concern the selected immutable artifacts, not canonical DB coverage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticTierCoverage {
    pub generation: ArtifactGenerationIdentityV1,
    pub selected_shards: usize,
    pub witnessed_live_passages: u64,
    pub retrieved_candidates: usize,
    pub contributed_candidates: usize,
}

#[derive(Debug, Clone)]
pub struct SemanticSearchCoverage {
    pub requested_topology: RetrievalTopology,
    pub phase: SemanticResultPhase,
    /// None means not executed in this phase, not a measured zero.
    pub fast: Option<SemanticTierCoverage>,
    pub quality: Option<SemanticTierCoverage>,
}

/// Results retain the same sealed owners until the batch is dropped. These
/// witnesses still do not authorize reading message bodies from a newer DB.
#[derive(Debug, Clone)]
pub struct SemanticSearchBatch {
    reader: SemanticGenerationReader,
    hits: Vec<SemanticPassageHit>,
    coverage: SemanticSearchCoverage,
    score_kind: SemanticScoreKind,
    execution: Vec<SemanticShardExecution>,
}

impl SemanticSearchBatch {
    pub fn hits(&self) -> &[SemanticPassageHit] {
        &self.hits
    }
    pub fn coverage(&self) -> &SemanticSearchCoverage {
        &self.coverage
    }
    pub fn score_kind(&self) -> SemanticScoreKind {
        self.score_kind
    }
    /// Per-shard execution for the phase(s) represented by this batch.
    pub fn execution(&self) -> &[SemanticShardExecution] {
        &self.execution
    }
    pub fn witness(&self, tier: TierKind, shard: usize) -> Option<&FsviV2Witness> {
        self.reader.witness(tier, shard)
    }
}

#[derive(Debug, Clone)]
struct Candidate {
    doc_id: String,
    document: SemanticDocId,
    score: f32,
    shard: usize,
    physical_index: usize,
    source_rank: usize,
}

// Greater means worse, so BinaryHeap::peek identifies the replaceable loser.
// Ties preserve selection shard order and the engine's local result order.
// Re-sorting shard-local ties by doc_id would be incorrect at the k boundary:
// the engine may already have excluded other equal-scoring physical rows.
impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .score
            .total_cmp(&self.score)
            .then_with(|| self.shard.cmp(&other.shard))
            .then_with(|| self.source_rank.cmp(&other.source_rank))
            .then_with(|| self.physical_index.cmp(&other.physical_index))
            .then_with(|| self.doc_id.cmp(&other.doc_id))
    }
}
impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Candidate {}

fn make_batch(
    reader: &SemanticGenerationReader,
    topology: RetrievalTopology,
    phase: SemanticResultPhase,
    fast: Option<TierResult>,
    quality: Option<TierResult>,
    k: usize,
) -> SemanticSearchBatch {
    let fast_count = fast.as_ref().map(|tier| tier.candidates.len());
    let quality_count = quality.as_ref().map(|tier| tier.candidates.len());
    let fused = fast.is_some() && quality.is_some();
    let execution: Vec<_> = fast
        .iter()
        .chain(quality.iter())
        .flat_map(|tier| tier.execution.iter().cloned())
        .collect();
    let used_ann = execution
        .iter()
        .any(|report| report.engine == SemanticShardEngine::NativeAnn);
    let fast = fast.map(|tier| tier.candidates);
    let quality = quality.map(|tier| tier.candidates);
    let mut merged = BTreeMap::<String, SemanticPassageHit>::new();
    let mut single = Vec::new();
    for (kind, candidates) in [(TierKind::Fast, fast), (TierKind::Quality, quality)] {
        for (rank, candidate) in candidates.into_iter().flatten().enumerate() {
            let source = SemanticHitSource {
                shard: candidate.shard,
                physical_index: candidate.physical_index,
                tier_rank: rank + 1,
                score: candidate.score,
            };
            let contribution = if fused {
                1.0 / (60.0 + (rank + 1) as f64)
            } else {
                f64::from(candidate.score)
            };
            if fused {
                let hit =
                    merged
                        .entry(candidate.doc_id.clone())
                        .or_insert_with(|| SemanticPassageHit {
                            doc_id: candidate.doc_id,
                            document: candidate.document,
                            ranking_score: 0.0,
                            fast: None,
                            quality: None,
                        });
                hit.ranking_score += contribution;
                match kind {
                    TierKind::Fast => hit.fast = Some(source),
                    TierKind::Quality => hit.quality = Some(source),
                }
            } else {
                let mut hit = SemanticPassageHit {
                    doc_id: candidate.doc_id,
                    document: candidate.document,
                    ranking_score: contribution,
                    fast: None,
                    quality: None,
                };
                match kind {
                    TierKind::Fast => hit.fast = Some(source),
                    TierKind::Quality => hit.quality = Some(source),
                }
                single.push(hit);
            }
        }
    }
    let mut hits = if fused {
        let mut hits: Vec<_> = merged.into_values().collect();
        hits.sort_by(|left, right| {
            right
                .ranking_score
                .total_cmp(&left.ranking_score)
                .then_with(|| left.doc_id.cmp(&right.doc_id))
        });
        hits
    } else {
        single
    };
    hits.truncate(k);
    let coverage_for = |kind: TierKind, count: Option<usize>| {
        count.and_then(|retrieved_candidates| {
            reader.tier(kind).map(|tier| SemanticTierCoverage {
                generation: tier.binding.generation(),
                selected_shards: tier.shards.len(),
                witnessed_live_passages: tier.live_count,
                retrieved_candidates,
                contributed_candidates: hits
                    .iter()
                    .filter(|hit| match kind {
                        TierKind::Fast => hit.fast.is_some(),
                        TierKind::Quality => hit.quality.is_some(),
                    })
                    .count(),
            })
        })
    };
    let coverage = SemanticSearchCoverage {
        requested_topology: topology,
        phase,
        fast: coverage_for(TierKind::Fast, fast_count),
        quality: coverage_for(TierKind::Quality, quality_count),
    };
    SemanticSearchBatch {
        reader: reader.clone(),
        hits,
        coverage,
        score_kind: if fused {
            SemanticScoreKind::ReciprocalRankFusion
        } else if used_ann {
            SemanticScoreKind::AnnRescored
        } else {
            SemanticScoreKind::Exact
        },
        execution,
    }
}

#[cfg(test)]
mod tests;
