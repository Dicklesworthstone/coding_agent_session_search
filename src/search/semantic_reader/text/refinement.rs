//! Request-local fast-tier reuse for current-source progressive refinement.
//!
//! The caller owns one stable original filter and monotonically adds rejected
//! passage IDs. Its exclusion revision changes whenever that set grows. Cached
//! candidates never cross requests, and a changed revision forces fresh ranking.
//! Store the real TierResult, including physical rows and execution evidence;
//! do not reconstruct candidates from a projected/fused output page.

use super::*;

#[derive(Default)]
pub(in crate::search::semantic_reader) struct FastTierCache {
    entry: Option<Entry>,
    reuses: usize,
}

struct Entry {
    reader: SemanticGenerationReader,
    text: String,
    producer: EmbeddingIdentityBundleV1,
    k: usize,
    policy: Option<(usize, usize)>,
    exclusion_revision: usize,
    result: TierResult,
}

impl FastTierCache {
    pub(in crate::search::semantic_reader) fn reuses(&self) -> usize {
        self.reuses
    }
}

fn policy_key(policy: Option<AnnSearchPolicy>) -> Option<(usize, usize)> {
    policy.map(|value| (value.initial_candidates, value.max_candidates))
}

impl Entry {
    fn matches(
        &self,
        search: &TextSemanticSearch<'_, '_>,
        k: usize,
        policy: Option<AnnSearchPolicy>,
        exclusion_revision: usize,
    ) -> bool {
        let same_tier = |left: &Option<Arc<AdmittedTier>>, right: &Option<Arc<AdmittedTier>>| {
            match (left, right) {
                (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                (None, None) => true,
                _ => false,
            }
        };
        self.exclusion_revision == exclusion_revision
            && self.k == k
            && self.policy == policy_key(policy)
            && self.text == search.text
            && search.fast.as_ref().is_some_and(|fast| fast.identity == self.producer)
            && self.reader.generation == search.reader.generation
            && same_tier(&self.reader.fast, &search.reader.fast)
            && same_tier(&self.reader.quality, &search.reader.quality)
            && Arc::ptr_eq(&self.reader.ann, &search.reader.ann)
    }
}

impl TextSemanticSearch<'_, '_> {
    /// Internal continuation operation. The current-source stream supplies one
    /// fixed request/filter and its append-only exclusion revision. Check both
    /// producers even when no fast work is needed. A cached ranking is not a
    /// source certificate: the caller must rehydrate the entire resulting phase.
    pub(in crate::search::semantic_reader) fn current_phase(
        &self,
        cache: &mut FastTierCache,
        refined: bool,
        k: usize,
        filter: &dyn SearchFilter,
        policy: Option<AnnSearchPolicy>,
        exclusion_revision: usize,
    ) -> SemanticReaderResult<SemanticSearchBatch> {
        if self.fast.is_none() || self.quality.is_none() {
            return Err(SemanticReaderError::ProgressiveRequiresBothTiers);
        }
        if let Some(policy) = policy {
            policy.validate()?;
        }
        self.revalidate()?;
        let reusable = refined && cache.entry.as_ref().is_some_and(|entry| {
            entry.matches(self, k, policy, exclusion_revision)
        });
        let fast = if reusable {
            let entry = cache.entry.as_ref().ok_or(SemanticReaderError::InvalidCandidate)?;
            cache.reuses = cache.reuses.saturating_add(1);
            entry.result.clone()
        } else {
            // Release old candidates BEFORE the replacement scan. Only the
            // existing reader's shared image owners remain, never another slab.
            cache.entry = None;
            let fast = self.search_tier(TierKind::Fast, k, Some(filter), policy)?
                .ok_or(SemanticReaderError::MissingTier(TierKind::Fast))?;
            self.revalidate()?;
            cache.entry = Some(Entry {
                reader: self.reader.clone(),
                text: self.text.to_owned(),
                producer: self.fast.as_ref()
                    .ok_or(SemanticReaderError::MissingTier(TierKind::Fast))?.identity.clone(),
                k,
                policy: policy_key(policy),
                exclusion_revision,
                result: fast.clone(),
            });
            fast
        };
        let quality = if refined {
            Some(self.search_tier(TierKind::Quality, k, Some(filter), policy)?
                .ok_or(SemanticReaderError::MissingTier(TierKind::Quality))?)
        } else {
            None
        };
        self.revalidate()?;
        Ok(make_batch(
            self.reader,
            self.topology,
            if refined { SemanticResultPhase::Refined } else { SemanticResultPhase::Initial },
            Some(fast),
            quality,
            k,
        ))
    }
}
