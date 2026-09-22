//! Text-to-retrieval over the already admitted immutable generation.
//!
//! Callers supply actual producer owners, not model names or artifact paths.
//! Every requested producer is checked before the first inference. Progressive
//! queries defer quality inference AND independent quality retrieval until the
//! second iterator step. Identity inspection may load a caller's lazy model;
//! this module never downloads, discovers, or substitutes a model.
//!
//! These synchronous calls do not preempt a running embedder or vector scan.
//! The caller remains responsible for process deadlines and total memory limits.

use frankensearch::core::generation::{EmbeddingIdentityBundleV1, EmbeddingSpaceKindV1};

use super::*;
use crate::search::embedder::Embedder;

/// Bound the borrowed input before inspecting producers or running inference.
pub const MAX_TEXT_QUERY_BYTES: usize = 4_096;

/// Explicit producer selection. No quality-to-fast or semantic-to-hash fallback.
/// Supplying a hash producer explicitly keeps the result's HashControl topology.
#[derive(Clone, Copy)]
pub enum TextQueryProducers<'model> {
    Fast(&'model dyn Embedder),
    Quality(&'model dyn Embedder),
    Progressive {
        fast: &'model dyn Embedder,
        quality: &'model dyn Embedder,
    },
}

impl std::fmt::Debug for TextQueryProducers<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Fast(_) => "TextQueryProducers::Fast",
            Self::Quality(_) => "TextQueryProducers::Quality",
            Self::Progressive { .. } => "TextQueryProducers::Progressive",
        })
    }
}

struct CheckedProducer<'model> {
    producer: &'model dyn Embedder,
    identity: EmbeddingIdentityBundleV1,
}

fn invalid(reason: &'static str) -> SemanticReaderError {
    SearchError::InvalidConfig {
        field: "semantic_text_query".into(),
        value: "redacted".into(),
        reason: reason.into(),
    }
    .into()
}

impl<'model> CheckedProducer<'model> {
    fn new(
        producer: &'model dyn Embedder,
        expected: &AdmittedTier,
        kind: TierKind,
    ) -> SemanticReaderResult<Self> {
        let identity = producer.identity()?.clone();
        identity.validate()?;
        let artifact = &expected.binding.frozen_identity().identity;
        // At the index seam storage is intentionally different: persisted
        // quantization versus in-memory producer output. Space, input and the
        // exact attested producer still have to match before doing any work.
        if identity.space != artifact.space || identity.input != artifact.input {
            return Err(invalid(
                "producer space or input contract differs from the selected tier",
            ));
        }
        if identity.producer != artifact.producer {
            return Err(SemanticReaderError::ForeignProducer(kind));
        }
        if producer.dimension() != expected.binding.dimension() {
            return Err(SearchError::DimensionMismatch {
                expected: expected.binding.dimension(),
                found: producer.dimension(),
            }
            .into());
        }
        Ok(Self { producer, identity })
    }

    fn check_current(&self) -> SemanticReaderResult<()> {
        if self.producer.identity()? != &self.identity
            || self.producer.dimension() != self.identity.space.dimension as usize
        {
            return Err(invalid(
                "producer identity changed after text-query admission",
            ));
        }
        Ok(())
    }

    fn embed(&self, text: &str) -> SemanticReaderResult<BoundQueryEmbedding> {
        self.check_current()?;
        // Use the production identity-bearing boundary, including overrides
        // such as attested daemon producers. Never stamp expected metadata on
        // an unbound vector, or accept a new identity after the call returns.
        let bound = self.producer.embed_bound_sync(text)?;
        bound.validate()?;
        self.check_current()?;
        if bound.identity != self.identity {
            return Err(invalid(
                "embedding response does not match the admitted producer",
            ));
        }
        Ok(BoundQueryEmbedding::new(bound.values, bound.identity)?)
    }
}

/// A text request bound to immutable readers and checked producer owners.
/// Cloning/replacing publication paths cannot alter this borrowed generation.
pub struct TextSemanticSearch<'reader, 'request> {
    reader: &'reader SemanticGenerationReader,
    text: &'request str,
    fast: Option<CheckedProducer<'request>>,
    quality: Option<CheckedProducer<'request>>,
    topology: RetrievalTopology,
}

impl std::fmt::Debug for TextSemanticSearch<'_, '_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Query text and producer diagnostics can contain private paths/text.
        formatter
            .debug_struct("TextSemanticSearch")
            .field("generation", &self.reader.generation())
            .field("requested_topology", &self.topology)
            .finish_non_exhaustive()
    }
}

impl SemanticGenerationReader {
    /// Validate ALL selected tiers and producers before inference becomes
    /// callable. A bad quality binding therefore cannot spend fast inference.
    /// Producer identity inspection is allowed to initialize a supplied lazy
    /// model, but never executes its embed method here. Input is not rewritten.
    pub fn activate_text<'reader, 'request>(
        &'reader self,
        text: &'request str,
        producers: TextQueryProducers<'request>,
    ) -> SemanticReaderResult<TextSemanticSearch<'reader, 'request>> {
        if text.trim().is_empty() || text.len() > MAX_TEXT_QUERY_BYTES {
            return Err(invalid(
                "query must be nonempty and at most 4096 UTF-8 bytes",
            ));
        }
        let (fast, quality) = match producers {
            TextQueryProducers::Fast(fast) => (Some(fast), None),
            TextQueryProducers::Quality(quality) => (None, Some(quality)),
            TextQueryProducers::Progressive { fast, quality } => (Some(fast), Some(quality)),
        };
        // Missing-tier rejection precedes even identity inspection, including
        // that of an otherwise valid first producer.
        for (kind, producer) in [(TierKind::Fast, fast), (TierKind::Quality, quality)] {
            if producer.is_some() && self.tier(kind).is_none() {
                return Err(SemanticReaderError::MissingTier(kind));
            }
        }
        let check = |kind, producer: Option<&'request dyn Embedder>| {
            producer
                .map(|producer| {
                    CheckedProducer::new(
                        producer,
                        self.tier(kind)
                            .ok_or(SemanticReaderError::MissingTier(kind))?,
                        kind,
                    )
                })
                .transpose()
        };
        let fast = check(TierKind::Fast, fast)?;
        let quality = check(TierKind::Quality, quality)?;
        // The same request-shape law as TieredQueryEmbeddings. This describes
        // requested capabilities, NOT realized coverage or semantic readiness.
        let topology = if fast
            .iter()
            .chain(&quality)
            .any(|checked| checked.identity.space.kind == EmbeddingSpaceKindV1::HashControl)
        {
            RetrievalTopology::HashControl
        } else {
            match (&fast, &quality) {
                (Some(_), Some(_)) => RetrievalTopology::FullProgressive,
                (Some(_), None) => RetrievalTopology::FastOnly,
                (None, Some(_)) => RetrievalTopology::QualityOnly,
                (None, None) => return Err(SemanticReaderError::NoTiers),
            }
        };
        Ok(TextSemanticSearch {
            reader: self,
            text,
            fast,
            quality,
            topology,
        })
    }
}

impl<'reader, 'request> TextSemanticSearch<'reader, 'request> {
    fn revalidate(&self) -> SemanticReaderResult<()> {
        for producer in self.fast.iter().chain(&self.quality) {
            producer.check_current()?;
        }
        Ok(())
    }

    fn search_tier(
        &self,
        kind: TierKind,
        k: usize,
        filter: Option<&dyn SearchFilter>,
        policy: Option<AnnSearchPolicy>,
    ) -> SemanticReaderResult<Option<TierResult>> {
        let producer = match kind {
            TierKind::Fast => self.fast.as_ref(),
            TierKind::Quality => self.quality.as_ref(),
        };
        let Some(producer) = producer else {
            return Ok(None);
        };
        let tier = self
            .reader
            .tier(kind)
            .ok_or(SemanticReaderError::MissingTier(kind))?;
        if k == 0 || tier.live_count == 0 {
            // A validated zero request / witnessed empty tier needs no model
            // inference. Preserve per-shard execution provenance as Skipped.
            return Ok(Some(TierResult {
                candidates: Vec::new(),
                execution: (0..tier.shards.len())
                    .map(|shard| SemanticShardExecution {
                        tier: kind,
                        shard,
                        engine: SemanticShardEngine::Skipped,
                        fallback_reason: None,
                        graph_sha256: None,
                        ann_windows: 0,
                        candidate_rows: 0,
                        final_candidate_limit: 0,
                        returned_candidates: 0,
                    })
                    .collect(),
            }));
        }
        let query = producer.embed(self.text)?;
        // Retain the existing bound-vector activation guard as well as the
        // pre-inference checks; the storage seam remains authoritative.
        tier.activate(&query, kind)?;
        tier.search(kind, &query, k, filter, &self.reader.ann, policy)
            .map(Some)
    }

    /// Embed the text once per requested nonempty tier, then independently
    /// retrieve and fuse. QualityOnly never executes a fast producer or shard.
    pub fn search(
        &self,
        k: usize,
        filter: Option<&dyn SearchFilter>,
    ) -> SemanticReaderResult<SemanticSearchBatch> {
        self.search_impl(k, filter, None)
    }

    /// Use only already admitted native graphs, with the existing exact fallback.
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
        self.revalidate()?;
        let fast = self.search_tier(TierKind::Fast, k, filter, policy)?;
        let quality = self.search_tier(TierKind::Quality, k, filter, policy)?;
        self.revalidate()?;
        Ok(make_batch(
            self.reader,
            self.topology,
            SemanticResultPhase::Complete,
            fast,
            quality,
            k,
        ))
    }

    /// Lazily embed/retrieve fast on the first step and quality on the second.
    /// Dropping the iterator after Initial avoids ALL quality inference/search.
    /// Caller-provided filter behavior must remain stable across both steps.
    pub fn progressive<'call>(
        &'call self,
        k: usize,
        filter: Option<&'call dyn SearchFilter>,
    ) -> SemanticReaderResult<ProgressiveTextSemanticSearch<'call, 'reader, 'request>> {
        self.progressive_impl(k, filter, None)
    }

    pub fn progressive_with_ann<'call>(
        &'call self,
        k: usize,
        filter: Option<&'call dyn SearchFilter>,
        policy: AnnSearchPolicy,
    ) -> SemanticReaderResult<ProgressiveTextSemanticSearch<'call, 'reader, 'request>> {
        policy.validate()?;
        self.progressive_impl(k, filter, Some(policy))
    }

    fn progressive_impl<'call>(
        &'call self,
        k: usize,
        filter: Option<&'call dyn SearchFilter>,
        policy: Option<AnnSearchPolicy>,
    ) -> SemanticReaderResult<ProgressiveTextSemanticSearch<'call, 'reader, 'request>> {
        if self.fast.is_none() || self.quality.is_none() {
            return Err(SemanticReaderError::ProgressiveRequiresBothTiers);
        }
        self.revalidate()?;
        Ok(ProgressiveTextSemanticSearch {
            search: self,
            k,
            filter,
            policy,
            state: ProgressiveState::Initial,
        })
    }
}

/// An inference or retrieval failure terminates the stream. An Initial batch
/// remains valid evidence for its fast phase, never a successful refined result.
pub struct ProgressiveTextSemanticSearch<'call, 'reader, 'request> {
    search: &'call TextSemanticSearch<'reader, 'request>,
    k: usize,
    filter: Option<&'call dyn SearchFilter>,
    policy: Option<AnnSearchPolicy>,
    state: ProgressiveState,
}

impl std::fmt::Debug for ProgressiveTextSemanticSearch<'_, '_, '_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProgressiveTextSemanticSearch")
            .field("search", &self.search)
            .field("k", &self.k)
            .finish_non_exhaustive()
    }
}

impl Iterator for ProgressiveTextSemanticSearch<'_, '_, '_> {
    type Item = SemanticReaderResult<SemanticSearchBatch>;

    fn next(&mut self) -> Option<Self::Item> {
        let state = std::mem::replace(&mut self.state, ProgressiveState::Done);
        if matches!(&state, ProgressiveState::Done) {
            return None;
        }
        if let Err(error) = self.search.revalidate() {
            return Some(Err(error));
        }
        match state {
            ProgressiveState::Initial => {
                let result = self
                    .search
                    .search_tier(TierKind::Fast, self.k, self.filter, self.policy)
                    .and_then(|tier| tier.ok_or(SemanticReaderError::MissingTier(TierKind::Fast)))
                    .and_then(|tier| {
                        self.search.revalidate()?;
                        Ok(tier)
                    });
                Some(result.map(|fast| {
                    let batch = make_batch(
                        self.search.reader,
                        self.search.topology,
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
                    .and_then(|tier| {
                        tier.ok_or(SemanticReaderError::MissingTier(TierKind::Quality))
                    })
                    .and_then(|tier| {
                        self.search.revalidate()?;
                        Ok(tier)
                    });
                Some(result.map(|quality| {
                    make_batch(
                        self.search.reader,
                        self.search.topology,
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

impl std::iter::FusedIterator for ProgressiveTextSemanticSearch<'_, '_, '_> {}

#[cfg(test)]
mod tests;
