//! Publication-selected retrieval, rather than filename-based vector discovery.
//!
//! The existing manifest loader supplies selection authority, not a serving
//! handle. This adapter admits each vector against that manifest's complete
//! identity and whole-image digest, then RETAINS that same sealed owner. Header
//! inspection supplies only the full-width generation needed by FSVI admission;
//! it is untrusted until the admitted image matches the manifest's SHA-256.
//!
//! The expected corpus must come from the caller's pinned canonical snapshot.
//! This module does not derive database identity from counts, filenames or the
//! manifest being read. It never opens/migrates a database, downloads a model,
//! repairs an index, or publishes a pointer. Manifest v1 selects one vector per
//! tier; v2 selects a complete, explicitly ordered shard set. Prototype shard
//! ledgers and adjacent ANN files are never selection authority.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

mod coverage;
mod live_docset;
pub mod text;

use frankensearch::core::TieredQueryEmbeddings;
use frankensearch::core::filter::SearchFilter;
use frankensearch::index::{
    FsviAdmissionError, FsviInspection, FsviV2IdentityBinding, ValidatedFsviBytes, VectorIndex,
};

use super::ann::{AnnAdmissionBudget, AnnSearchPolicy, SemanticAnnAdmission};

use super::{
    ActivatedSemanticSearch, AdmittedTier, SemanticGenerationReader, SemanticReaderError,
    SemanticReaderResult, SemanticSearchBatch, canonical_document,
};
use crate::search::semantic_manifest::selection::{
    SelectedSemanticVectors, SemanticSelectionMetadata,
};
use crate::search::semantic_manifest::{
    SemanticArtifactRole, SemanticCorpusSnapshotIdentity, SemanticCurrentPointerV1,
    SemanticGenerationArtifact, SemanticGenerationError, SemanticGenerationManifestV1, TierKind,
};

#[cfg(all(test, any(target_os = "linux", target_os = "android")))]
use crate::search::semantic_manifest::load_current_semantic_generation;

/// Admission limits on manifest-declared vector images, not a total RSS limit.
///
/// Checked after bounded pointer/manifest validation but BEFORE any artifact
/// is opened or hashed. The upstream admission API performs its own complete
/// file read. Concurrent source-file growth, parser scratch, graph bytes and a
/// retained previous generation during refresh are outside this preflight bound.
#[derive(Debug, Clone, Copy)]
pub struct SemanticSelectionBudget {
    pub max_declared_vector_bytes: u64,
}

impl Default for SemanticSelectionBudget {
    fn default() -> Self {
        Self {
            max_declared_vector_bytes: 512 * 1024 * 1024,
        }
    }
}

impl SemanticSelectionBudget {
    fn check(self, manifest: &SemanticGenerationManifestV1) -> SemanticSelectionResult<()> {
        let declared = manifest
            .artifacts
            .iter()
            .filter(|artifact| {
                matches!(
                    artifact.role,
                    SemanticArtifactRole::FastVector | SemanticArtifactRole::QualityVector
                )
            })
            .try_fold(0_u64, |total, artifact| {
                total.checked_add(artifact.size_bytes)
            })
            .ok_or(SemanticSelectionError::BudgetExceeded)?;
        if declared > self.max_declared_vector_bytes || usize::try_from(declared).is_err() {
            return Err(SemanticSelectionError::BudgetExceeded);
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SemanticSelectionError {
    #[error(transparent)]
    Publication(#[from] SemanticGenerationError),
    #[error(transparent)]
    Reader(#[from] SemanticReaderError),
    #[error(transparent)]
    Index(#[from] frankensearch::SearchError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("selected {role:?} requires an identity-complete FSVI v2 artifact")]
    UnsupportedArtifact { role: SemanticArtifactRole },
    #[error("selected {role:?} admission failed: {source}")]
    Admission {
        role: SemanticArtifactRole,
        #[source]
        source: Box<FsviAdmissionError>,
    },
    #[error("selected {role:?} disagrees with its admitted image: {field}")]
    ArtifactMismatch {
        role: SemanticArtifactRole,
        field: &'static str,
    },
    #[error("selected vector images exceed the declared-byte admission budget")]
    BudgetExceeded,
    #[error("current semantic publication changed during reader admission")]
    SelectionChanged,
    #[error("refresh must advance the selection epoch or retain the exact same publication")]
    StaleSelection,
}

pub type SemanticSelectionResult<T> = Result<T, SemanticSelectionError>;

/// Immutable publication identity retained with every selected result batch.
/// There is no public constructor from caller-supplied paths or receipts.
#[derive(Debug)]
pub struct SemanticSelectionIdentity {
    pointer: SemanticCurrentPointerV1,
    manifest: SemanticGenerationManifestV1,
}

impl SemanticSelectionIdentity {
    pub fn pointer(&self) -> &SemanticCurrentPointerV1 {
        &self.pointer
    }
    pub fn manifest(&self) -> &SemanticGenerationManifestV1 {
        &self.manifest
    }
    pub fn corpus(&self) -> &SemanticCorpusSnapshotIdentity {
        &self.manifest.corpus
    }
}

/// A reader selected by `current.json`, with fully admitted vector owners.
///
/// Clones share both artifact images and the complete publication identity.
/// Selection does not imply complete coverage: consult the manifest's per-tier
/// covered/selected counts. Hash-control queries remain hash-control queries.
/// Opening validates mandatory vector files only; missing/corrupt optional ANN
/// files do not prevent exact retrieval. Graph admission is explicit in with_ann().
/// Use load_current_semantic_generation for a strict audit of ALL selected files.
#[derive(Debug, Clone)]
pub struct SelectedSemanticGeneration {
    data_dir: PathBuf,
    identity: Arc<SemanticSelectionIdentity>,
    reader: SemanticGenerationReader,
    // Opt-in graph loading is sticky across publication refreshes, but never
    // changes the default exact query policy or mutates earlier result batches.
    ann_budget: Option<AnnAdmissionBudget>,
}

impl SelectedSemanticGeneration {
    pub fn open_current(
        data_dir: &Path,
        expected_corpus: &SemanticCorpusSnapshotIdentity,
        budget: SemanticSelectionBudget,
    ) -> SemanticSelectionResult<Self> {
        Self::open_current_with_checkpoint(data_dir, expected_corpus, budget, || {})
    }

    fn open_current_with_checkpoint(
        data_dir: &Path,
        expected_corpus: &SemanticCorpusSnapshotIdentity,
        budget: SemanticSelectionBudget,
        after_admission: impl FnOnce(),
    ) -> SemanticSelectionResult<Self> {
        // Freeze relative resolution before the first read. Do not canonicalize
        // away symlinks before the existing manifest path validator sees them.
        let data_dir = if data_dir.is_absolute() {
            data_dir.to_path_buf()
        } else {
            std::env::current_dir()?.join(data_dir)
        };
        let metadata = SemanticSelectionMetadata::read(&data_dir, Some(expected_corpus))?;
        budget.check(&metadata.manifest)?;
        let selected = metadata.preflight_vectors(&data_dir)?;

        let fast = admit_tier(&selected, SemanticArtifactRole::FastVector)?;
        let quality = admit_tier(&selected, SemanticArtifactRole::QualityVector)?;
        let generation = fast
            .as_ref()
            .or(quality.as_ref())
            .ok_or(SemanticReaderError::NoTiers)?
            .binding
            .generation();
        if let (Some(fast), Some(quality)) = (&fast, &quality) {
            if fast.binding.generation() != quality.binding.generation() {
                return Err(SemanticReaderError::MixedGeneration.into());
            }
            if selected.manifest.schema_version
                == crate::search::semantic_manifest::SEMANTIC_SHARDED_GENERATION_MANIFEST_SCHEMA_VERSION
            {
                coverage::validate(fast, quality, selected.manifest.corpus.document_count)?;
            }
            let fast_images: HashSet<_> = fast
                .shards
                .iter()
                .map(|shard| shard.witness().whole_image_sha256)
                .collect();
            if quality
                .shards
                .iter()
                .any(|shard| fast_images.contains(&shard.witness().whole_image_sha256))
            {
                return Err(SemanticReaderError::ArtifactRoleAlias.into());
            }
        }
        let reader = SemanticGenerationReader {
            fast: fast.map(Arc::new),
            quality: quality.map(Arc::new),
            generation,
            ann: Arc::new(super::ann::AnnSelection::default()),
        };
        // Authenticate each serving owner exactly once, then close the handoff
        // against the originally opened file identities before exposing a reader.
        // Metadata-only selection is never itself a content-validation receipt.
        selected.verify_paths()?;
        after_admission();
        // All serving bytes are now owned and witness-checked. Revalidate the
        // pointer and manifest through the SAME metadata parser, not every
        // artifact pathname again. A later path rewrite cannot change a sealed
        // owner. An epoch-changing A -> B -> A selection is still rejected.
        let confirmed = SemanticSelectionMetadata::read(&data_dir, Some(expected_corpus))?;
        if confirmed.pointer != selected.pointer || confirmed.manifest != selected.manifest {
            return Err(SemanticSelectionError::SelectionChanged);
        }
        Ok(Self {
            data_dir,
            identity: Arc::new(SemanticSelectionIdentity {
                pointer: selected.pointer,
                manifest: selected.manifest,
            }),
            reader,
            ann_budget: None,
        })
    }

    /// Opt into graphs named by this already-selected immutable manifest.
    /// Never rediscover a graph beside a vector or reopen a serving vector.
    /// Rejections remain per-tier exact fallback; inspect ann_admission() and
    /// each batch's execution() rather than inferring acceleration from files.
    ///
    /// The selection remains the one this handle admitted, even when current.json
    /// advances during graph loading. refresh_current() is the explicit boundary
    /// for adopting a successor. Ordinary search() remains exact after this call.
    pub fn with_ann(mut self, budget: AnnAdmissionBudget) -> SemanticSelectionResult<Self> {
        let directory = self.identity.manifest.generation_dir(&self.data_dir)?;
        self.reader = self
            .reader
            .with_manifest_ann(&self.identity.manifest, &directory, budget);
        self.ann_budget = Some(budget);
        Ok(self)
    }

    /// Drop this handle's graph selection and opt out of graph loading on refresh.
    /// Existing clones and batches keep their own graph/vector owners alive.
    pub fn without_ann(mut self) -> Self {
        self.reader.ann = Arc::new(super::ann::AnnSelection::default());
        self.ann_budget = None;
        self
    }

    /// None means the tier is absent, not that a present tier has zero work.
    pub fn ann_admission(&self, tier: TierKind) -> Option<SemanticAnnAdmission> {
        self.ann_admission_at(tier, 0)
    }

    pub fn ann_admission_at(&self, tier: TierKind, shard: usize) -> Option<SemanticAnnAdmission> {
        self.reader.ann_admission(tier, shard)
    }

    pub fn shard_count(&self, tier: TierKind) -> usize {
        self.reader.tier(tier).map_or(0, |tier| tier.shards.len())
    }

    /// Revalidate a successor before touching the current handle. An unchanged
    /// selection reuses its sealed vector/graph owners after a bounded metadata
    /// recheck; it does not reread artifact paths or allocate duplicate images.
    /// Use load_current_semantic_generation for a fresh audit of files on disk.
    ///
    /// Selection epochs, NOT vector-build sequence numbers, order refreshes.
    /// An explicitly published rollback to an older build is valid at a newer
    /// selection epoch. Previously returned batches retain their old owners.
    /// Explicit ANN opt-in and its admission budget carry forward to the new
    /// manifest; old graphs are never attached to a successor's vector owners.
    pub fn refresh_current(
        &mut self,
        expected_corpus: &SemanticCorpusSnapshotIdentity,
        budget: SemanticSelectionBudget,
    ) -> SemanticSelectionResult<bool> {
        let selected = SemanticSelectionMetadata::read(&self.data_dir, Some(expected_corpus))?;
        budget.check(&selected.manifest)?;
        if !self.check_refresh_selection(&selected.pointer, &selected.manifest)? {
            return Ok(false);
        }

        // Metadata is not an admission receipt. Every genuinely new selection
        // still crosses complete vector and sealed-owner validation. Recheck
        // ordering afterward too: current.json may change while it is opened.
        let candidate = Self::open_current(&self.data_dir, expected_corpus, budget)?;
        if !self
            .check_refresh_selection(&candidate.identity.pointer, &candidate.identity.manifest)?
        {
            return Ok(false);
        }
        let candidate = match self.ann_budget {
            Some(ann_budget) => candidate.with_ann(ann_budget)?,
            None => candidate,
        };
        *self = candidate;
        Ok(true)
    }

    fn check_refresh_selection(
        &self,
        next: &SemanticCurrentPointerV1,
        manifest: &SemanticGenerationManifestV1,
    ) -> SemanticSelectionResult<bool> {
        let previous = &self.identity.pointer;
        if next.selection_epoch < previous.selection_epoch
            || (next.selection_epoch == previous.selection_epoch
                && (next != previous || manifest != &self.identity.manifest))
        {
            return Err(SemanticSelectionError::StaleSelection);
        }
        Ok(next != previous)
    }

    pub fn selection(&self) -> &SemanticSelectionIdentity {
        &self.identity
    }

    pub fn witness(&self, tier: TierKind) -> Option<&frankensearch::index::FsviV2Witness> {
        self.witness_at(tier, 0)
    }

    pub fn witness_at(
        &self,
        tier: TierKind,
        shard: usize,
    ) -> Option<&frankensearch::index::FsviV2Witness> {
        self.reader.witness(tier, shard)
    }

    pub fn activate<'reader, 'query>(
        &'reader self,
        queries: &'query TieredQueryEmbeddings,
    ) -> SemanticReaderResult<SelectedSemanticSearch<'reader, 'query>> {
        Ok(SelectedSemanticSearch {
            search: self.reader.activate(queries)?,
            identity: Arc::clone(&self.identity),
        })
    }
}

/// Activated queries cannot be reassigned to a different publication.
#[derive(Debug)]
pub struct SelectedSemanticSearch<'reader, 'query> {
    search: ActivatedSemanticSearch<'reader, 'query>,
    identity: Arc<SemanticSelectionIdentity>,
}

impl SelectedSemanticSearch<'_, '_> {
    pub fn search(
        &self,
        k: usize,
        filter: Option<&dyn SearchFilter>,
    ) -> SemanticReaderResult<SelectedSemanticBatch> {
        self.search
            .search(k, filter)
            .map(|batch| SelectedSemanticBatch {
                batch,
                identity: Arc::clone(&self.identity),
            })
    }

    /// Request native ANN over graphs explicitly admitted with with_ann().
    /// The caller selects approximation separately from loading. Missing or
    /// rejected graphs, candidate limits and filtered underfill use the exact
    /// retained tier, with the actual engine recorded in batch().execution().
    pub fn search_with_ann(
        &self,
        k: usize,
        filter: Option<&dyn SearchFilter>,
        policy: AnnSearchPolicy,
    ) -> SemanticReaderResult<SelectedSemanticBatch> {
        self.search
            .search_with_ann(k, filter, policy)
            .map(|batch| SelectedSemanticBatch {
                batch,
                identity: Arc::clone(&self.identity),
            })
    }

    /// Lazy ANN-accelerated fast results followed by independent quality
    /// retrieval and rank fusion, all bound to the same publication. Stopping
    /// after Initial does not run quality retrieval; quality failure stays an
    /// error, never a falsely successful refinement. Graph admission itself is
    /// explicit and eager in with_ann(), not hidden inside iterator creation.
    pub fn progressive_with_ann<'call>(
        &'call self,
        k: usize,
        filter: Option<&'call dyn SearchFilter>,
        policy: AnnSearchPolicy,
    ) -> SemanticReaderResult<
        impl Iterator<Item = SemanticReaderResult<SelectedSemanticBatch>> + 'call,
    > {
        let identity = Arc::clone(&self.identity);
        Ok(self
            .search
            .progressive_with_ann(k, filter, policy)?
            .map(move |result| {
                result.map(|batch| SelectedSemanticBatch {
                    batch,
                    identity: Arc::clone(&identity),
                })
            }))
    }

    /// Preserve lazy fast/quality retrieval and attach the same publication
    /// identity to both phases. A quality failure remains an error, not a
    /// relabelled initial result. No inference is started by this iterator.
    pub fn progressive<'call>(
        &'call self,
        k: usize,
        filter: Option<&'call dyn SearchFilter>,
    ) -> SemanticReaderResult<
        impl Iterator<Item = SemanticReaderResult<SelectedSemanticBatch>> + 'call,
    > {
        let identity = Arc::clone(&self.identity);
        Ok(self.search.progressive(k, filter)?.map(move |result| {
            result.map(|batch| SelectedSemanticBatch {
                batch,
                identity: Arc::clone(&identity),
            })
        }))
    }
}

/// Passage results and publication provenance travel together. The underlying
/// batch retains its exact serving owners even after the selected reader drops.
#[derive(Debug)]
pub struct SelectedSemanticBatch {
    batch: SemanticSearchBatch,
    identity: Arc<SemanticSelectionIdentity>,
}

impl SelectedSemanticBatch {
    pub fn batch(&self) -> &SemanticSearchBatch {
        &self.batch
    }
    pub fn selection(&self) -> &SemanticSelectionIdentity {
        &self.identity
    }
}

fn admit_tier(
    selected: &SelectedSemanticVectors,
    role: SemanticArtifactRole,
) -> SemanticSelectionResult<Option<AdmittedTier>> {
    let artifacts: Vec<_> = selected.manifest.artifacts_for(role).collect();
    if artifacts.is_empty() {
        return Ok(None);
    }
    let mut shards = Vec::with_capacity(artifacts.len());
    let mut binding = None;
    let mut live_count = 0_u64;
    for artifact in &artifacts {
        if artifact.artifact_format != "fsvi-v2" {
            return Err(SemanticSelectionError::UnsupportedArtifact { role });
        }
        // All relative paths were validated by the authoritative loader. No
        // role-keyed lookup: that would silently drop all but one v2 shard.
        let path = selected.generation_dir.join(&artifact.relative_path);
        let FsviInspection::V2IdentityComplete(metadata) = VectorIndex::inspect(&path)? else {
            return Err(SemanticSelectionError::UnsupportedArtifact { role });
        };
        let generation = metadata
            .identity_v2
            .as_ref()
            .ok_or(SemanticSelectionError::UnsupportedArtifact { role })?
            .generation;
        let expected = FsviV2IdentityBinding::new(generation, artifact.embedding_identity.clone())?;
        if let Some(first) = binding.as_ref()
            && first != &expected
        {
            return Err(SemanticReaderError::MixedTierIdentity.into());
        }
        let owner = ValidatedFsviBytes::open_published(&path, &expected).map_err(|source| {
            SemanticSelectionError::Admission {
                role,
                source: Box::new(source),
            }
        })?;
        check_artifact(artifact, &owner)?;
        live_count = live_count
            .checked_add(owner.witness().live_count)
            .ok_or(SemanticReaderError::CountOverflow)?;
        binding.get_or_insert(expected);
        shards.push(Arc::new(owner));
    }

    // Physical FSVI rows use hash-key order. Manifest range bounds are the
    // lexical extrema of live IDs, not the first/last physical row positions.
    // The whole-tier witness is reconstructed separately in global FSVI order.
    let mut ids = HashSet::new();
    for (artifact, owner) in artifacts.iter().zip(&shards) {
        let mut first_live: Option<&str> = None;
        let mut last_live: Option<&str> = None;
        for position in 0..owner.record_count() {
            let id = owner.doc_id_at(position)?;
            canonical_document(id)?;
            if !owner.row(position)?.flags().is_live() {
                continue;
            }
            if !ids.insert(id) {
                return Err(SemanticReaderError::DuplicateDocument.into());
            }
            first_live = Some(first_live.map_or(id, |first| first.min(id)));
            last_live = Some(last_live.map_or(id, |last| last.max(id)));
        }
        if let Some(shard) = &artifact.shard
            && (first_live != Some(shard.first_document_id.as_str())
                || last_live != Some(shard.last_document_id.as_str()))
        {
            return Err(SemanticSelectionError::ArtifactMismatch {
                role,
                field: "shard_document_range",
            });
        }
    }
    if artifacts[0].shard.is_some()
        && live_count == selected.manifest.corpus.document_count
        && hex::encode(live_docset::digest(&shards, live_count)?)
            != selected.manifest.corpus.ordered_live_docset_sha256
    {
        return Err(SemanticSelectionError::ArtifactMismatch {
            role,
            field: "tier_live_docset",
        });
    }
    drop(ids);
    Ok(Some(AdmittedTier {
        binding: binding.expect("nonempty tier"),
        shards,
        live_count,
    }))
}

fn check_artifact(
    artifact: &SemanticGenerationArtifact,
    owner: &ValidatedFsviBytes,
) -> SemanticSelectionResult<()> {
    let witness = owner.witness();
    let mismatch = |field| SemanticSelectionError::ArtifactMismatch {
        role: artifact.role,
        field,
    };
    if hex::encode(witness.whole_image_sha256) != artifact.artifact_sha256 {
        return Err(mismatch("artifact_sha256"));
    }
    if witness.byte_len != artifact.size_bytes {
        return Err(mismatch("size_bytes"));
    }
    if witness.record_count != artifact.vector_slot_count {
        return Err(mismatch("vector_slot_count"));
    }
    if witness.live_count != artifact.live_vector_count
        || witness.live_count != artifact.covered_document_count
    {
        return Err(mismatch("covered_document_count"));
    }
    if witness.tombstone_count != artifact.tombstone_vector_count {
        return Err(mismatch("tombstone_vector_count"));
    }
    if hex::encode(witness.ordered_live_docset_digest) != artifact.covered_live_docset_sha256 {
        return Err(mismatch("covered_live_docset_sha256"));
    }
    if !owner.published_wal_absent() || artifact.wal_entry_count != 0 {
        return Err(SemanticReaderError::NonCanonicalDocuments.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
