//! Query embeddings belong to the producer admitted with one search context.
//!
//! Legacy FSVI admission still has its existing format/space checks. This module
//! binds inference and cache reuse to the producer's complete declared identity;
//! it does not manufacture an immutable identity for an older index or provider.

use std::num::NonZeroUsize;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use frankensearch::SearchError;
use frankensearch::core::EmbeddingIdentityBundleV1;
use lru::LruCache;

use crate::search::embedder::Embedder;

/// A legacy provider is recognized only by SyncEmbed's explicit default error.
/// Other identity failures are refusals, never permission to use raw inference.
fn declared_identity(embedder: &dyn Embedder) -> Result<Option<&EmbeddingIdentityBundleV1>> {
    match embedder.identity() {
        Ok(identity) => Ok(Some(identity)),
        Err(SearchError::InvalidConfig { field, reason, .. })
            if field == "sync_embedder.identity"
                && reason == "embedder did not supply a complete immutable identity bundle" =>
        {
            Ok(None)
        }
        Err(_) => bail!("semantic query producer identity is unavailable"),
    }
}

#[derive(Debug)]
pub(super) struct QueryProducer {
    id: String,
    dimension: usize,
    identity: Option<EmbeddingIdentityBundleV1>,
}

impl QueryProducer {
    fn capture(embedder: &dyn Embedder) -> Result<Self> {
        let id = embedder.id().to_owned();
        let dimension = embedder.dimension();
        if dimension == 0 {
            bail!("semantic query producer declares an empty vector");
        }
        let identity = declared_identity(embedder)?.cloned();
        if let Some(identity) = &identity {
            identity
                .validate()
                .map_err(|_| anyhow!("semantic query producer identity is invalid"))?;
            if usize::try_from(identity.space.dimension).ok() != Some(dimension) {
                bail!("semantic query producer dimension disagrees with its identity");
            }
        }
        let producer = Self {
            id,
            dimension,
            identity,
        };
        producer.validate(embedder)?;
        Ok(producer)
    }

    pub(super) fn id(&self) -> &str {
        &self.id
    }

    pub(super) fn dimension(&self) -> usize {
        self.dimension
    }

    /// Validate even for a cache hit. A display name and dimension cannot prove
    /// that a model revision, tokenizer, input contract or producer is unchanged.
    pub(super) fn validate(&self, embedder: &dyn Embedder) -> Result<()> {
        if embedder.id() != self.id || embedder.dimension() != self.dimension {
            bail!("semantic query producer changed after context admission");
        }
        if declared_identity(embedder)? != self.identity.as_ref() {
            bail!("semantic query producer identity changed after context admission");
        }
        Ok(())
    }

    pub(super) fn embed(&self, embedder: &dyn Embedder, text: &str) -> Result<Vec<f32>> {
        self.validate(embedder)?;
        let vector = if let Some(expected) = &self.identity {
            // Preserve custom bound dispatch; never label raw output with an
            // identity captured independently of the actual response.
            let outcome = embedder.embed_bound_sync(text);
            self.validate(embedder)?;
            let response = outcome.map_err(|error| anyhow!("embedding failed: {error}"))?;
            if &response.identity != expected {
                bail!("semantic query response identity differs from the admitted producer");
            }
            response
                .validate()
                .map_err(|_| anyhow!("semantic query producer returned an invalid bound vector"))?;
            response.values
        } else {
            // Existing external providers do not yet supply a complete bundle.
            // Preserve their explicit opt-in path without claiming identity
            // verification or caching an unknown embedding space.
            let outcome = embedder.embed_sync(text);
            self.validate(embedder)?;
            outcome.map_err(|error| anyhow!("embedding failed: {error}"))?
        };
        if vector.len() != self.dimension || vector.iter().any(|value| !value.is_finite()) {
            bail!("semantic query producer returned an invalid vector");
        }
        Ok(vector)
    }
}

pub(super) struct QueryCache {
    producer: Arc<QueryProducer>,
    embeddings: LruCache<String, Vec<f32>>,
}

impl QueryCache {
    pub(super) fn new(embedder: &dyn Embedder, capacity: NonZeroUsize) -> Result<Self> {
        Ok(Self {
            producer: Arc::new(QueryProducer::capture(embedder)?),
            embeddings: LruCache::new(capacity),
        })
    }

    pub(super) fn producer(&self) -> Arc<QueryProducer> {
        Arc::clone(&self.producer)
    }

    // Callers must validate the retained producer outside the semantic mutex
    // before exposing a hit. All insertions follow QueryProducer::embed.
    pub(super) fn get_cached(&mut self, canonical: &str) -> Option<Vec<f32>> {
        self.producer.identity.as_ref()?;
        self.embeddings.get(canonical).cloned()
    }

    pub(super) fn store(&mut self, canonical: &str, embedding: Vec<f32>) {
        if self.producer.identity.is_some() {
            self.embeddings.put(canonical.to_owned(), embedding);
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::search::embedder::EmbedderResult;
    use crate::search::hash_embedder::HashEmbedder;
    use frankensearch::core::IdentityBoundEmbedding;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    // Real hash inference with deliberately hostile identity/output wrappers.
    // This exercises admission, not MiniLM relevance or model authentication.
    pub(crate) struct ControlledEmbedder {
        hash: HashEmbedder,
        alternate: EmbeddingIdentityBundleV1,
        changed: AtomicBool,
        lost: AtomicBool,
        mode: AtomicUsize,
        raw_calls: AtomicUsize,
        bound_calls: AtomicUsize,
    }

    impl ControlledEmbedder {
        pub(crate) fn new() -> Self {
            let hash = HashEmbedder::new(2);
            let mut alternate = hash.identity().expect("hash identity").clone();
            alternate.space.immutable_revision = "changed-revision".to_owned();
            alternate.producer.space_fingerprint = alternate.space.fingerprint();
            alternate.validate().expect("valid different identity");
            Self {
                hash,
                alternate,
                changed: AtomicBool::new(false),
                lost: AtomicBool::new(false),
                mode: AtomicUsize::new(0),
                raw_calls: AtomicUsize::new(0),
                bound_calls: AtomicUsize::new(0),
            }
        }

        pub(crate) fn change_identity(&self) {
            self.changed.store(true, Ordering::SeqCst);
        }

        pub(crate) fn mode(&self, mode: usize) {
            self.mode.store(mode, Ordering::SeqCst);
        }

        pub(crate) fn calls(&self) -> usize {
            self.bound_calls.load(Ordering::SeqCst)
        }
    }

    impl Embedder for ControlledEmbedder {
        fn id(&self) -> &str {
            "test-fixed-2d"
        }

        fn dimension(&self) -> usize {
            2
        }

        fn is_semantic(&self) -> bool {
            false
        }

        fn category(&self) -> frankensearch::ModelCategory {
            frankensearch::ModelCategory::HashEmbedder
        }

        fn identity(&self) -> EmbedderResult<&EmbeddingIdentityBundleV1> {
            if self.lost.load(Ordering::SeqCst) {
                return Err(SearchError::InvalidConfig {
                    field: "sync_embedder.identity".into(),
                    value: self.id().into(),
                    reason: "embedder did not supply a complete immutable identity bundle".into(),
                });
            }
            if self.changed.load(Ordering::SeqCst) {
                Ok(&self.alternate)
            } else {
                self.hash.identity()
            }
        }

        fn embed_sync(&self, text: &str) -> EmbedderResult<Vec<f32>> {
            self.raw_calls.fetch_add(1, Ordering::SeqCst);
            self.hash.embed_sync(text)
        }

        fn embed_bound_sync(&self, text: &str) -> EmbedderResult<IdentityBoundEmbedding> {
            self.bound_calls.fetch_add(1, Ordering::SeqCst);
            let mut response = self.hash.embed_bound_sync(text)?;
            match self.mode.load(Ordering::SeqCst) {
                0 => {}
                1 => response.identity = self.alternate.clone(),
                2 => response.values.truncate(1),
                3 => response.values[0] = f32::NAN,
                4 => response.values[0] = f32::INFINITY,
                5 => self.change_identity(),
                6 => self.lost.store(true, Ordering::SeqCst),
                7 => response.identity.storage.dimension = 3,
                _ => unreachable!("unknown test response mode"),
            }
            Ok(response)
        }
    }

    fn capacity() -> NonZeroUsize {
        NonZeroUsize::new(4).expect("nonzero capacity")
    }

    #[test]
    fn bound_query_uses_the_actual_bound_response_and_preserves_hash_values() -> Result<()> {
        let embedder = ControlledEmbedder::new();
        let mut cache = QueryCache::new(&embedder, capacity())?;
        let producer = cache.producer();
        let vector = producer.embed(&embedder, "authentication retry")?;
        assert_eq!(vector, embedder.hash.embed_sync("authentication retry")?);
        assert_eq!(embedder.calls(), 1);
        assert_eq!(embedder.raw_calls.load(Ordering::SeqCst), 0);
        cache.store("authentication retry", vector.clone());
        producer.validate(&embedder)?;
        assert_eq!(cache.get_cached("authentication retry"), Some(vector));
        Ok(())
    }

    #[test]
    fn bound_query_rejects_wrong_space_malformed_and_nonfinite_responses() -> Result<()> {
        for mode in [1, 2, 3, 4, 7] {
            let embedder = ControlledEmbedder::new();
            let producer = QueryProducer::capture(&embedder)?;
            embedder.mode(mode);
            assert!(
                producer.embed(&embedder, "bounded query").is_err(),
                "invalid response mode {mode} must be rejected"
            );
            assert_eq!(embedder.calls(), 1);
            assert_eq!(embedder.raw_calls.load(Ordering::SeqCst), 0);
        }
        Ok(())
    }

    #[test]
    fn bound_query_rejects_mid_inference_identity_drift_without_downgrading() -> Result<()> {
        for mode in [5, 6] {
            let embedder = ControlledEmbedder::new();
            let producer = QueryProducer::capture(&embedder)?;
            embedder.mode(mode);
            assert!(producer.embed(&embedder, "bounded query").is_err());
            assert!(producer.embed(&embedder, "bounded query").is_err());
            assert_eq!(embedder.calls(), 1, "drift must stop the next inference");
            assert_eq!(embedder.raw_calls.load(Ordering::SeqCst), 0);
        }
        Ok(())
    }

    #[test]
    fn bound_query_pins_all_identity_contracts_even_at_the_same_name_and_dimension() -> Result<()> {
        for field in 0..7 {
            let mut embedder = ControlledEmbedder::new();
            embedder.alternate = embedder.hash.identity()?.clone();
            match field {
                0 => embedder.alternate.space.immutable_revision = "other-revision".into(),
                1 => embedder.alternate.space.tokenizer_fingerprint = "f".repeat(64),
                2 => embedder.alternate.space.model_preprocessing = "other-preprocessing".into(),
                3 => embedder.alternate.space.pooling = "other-pooling".into(),
                4 => {
                    embedder.alternate.input.canonicalization = "other-canonicalization".into();
                    embedder.alternate.space.input_contract_fingerprint =
                        embedder.alternate.input.fingerprint();
                }
                5 => embedder.alternate.producer.implementation_revision = "other-producer".into(),
                6 => embedder.alternate.storage.format = "in-memory-other-f32".into(),
                _ => unreachable!(),
            }
            embedder.alternate.producer.space_fingerprint = embedder.alternate.space.fingerprint();
            embedder.alternate.validate()?;
            let producer = QueryProducer::capture(&embedder)?;
            embedder.change_identity();
            assert!(producer.validate(&embedder).is_err(), "field {field}");
            assert!(producer.embed(&embedder, "same-name query").is_err());
            assert_eq!(embedder.calls(), 0);
        }
        Ok(())
    }

    struct LegacyEmbedder {
        vector: Vec<f32>,
    }

    impl Embedder for LegacyEmbedder {
        fn embed_sync(&self, _text: &str) -> EmbedderResult<Vec<f32>> {
            Ok(self.vector.clone())
        }
        fn dimension(&self) -> usize {
            2
        }
        fn id(&self) -> &str {
            "legacy-external-fixture"
        }
        fn is_semantic(&self) -> bool {
            true
        }
        fn category(&self) -> frankensearch::ModelCategory {
            frankensearch::ModelCategory::TransformerEmbedder
        }
    }

    #[test]
    fn legacy_query_is_validated_but_never_cached_as_a_verified_space() -> Result<()> {
        let embedder = LegacyEmbedder {
            vector: vec![1.0, 0.0],
        };
        let mut cache = QueryCache::new(&embedder, capacity())?;
        let producer = cache.producer();
        let vector = producer.embed(&embedder, "query")?;
        assert_eq!(vector, vec![1.0, 0.0]);
        cache.store("query", vector);
        assert!(cache.get_cached("query").is_none());
        assert!(cache.embeddings.is_empty());
        for vector in [vec![1.0], vec![f32::NAN, 0.0], vec![0.0, f32::INFINITY]] {
            let invalid = LegacyEmbedder { vector };
            assert!(producer.embed(&invalid, "query").is_err());
        }
        Ok(())
    }

    #[test]
    fn malformed_declared_identity_is_not_treated_as_a_legacy_provider() {
        let mut embedder = ControlledEmbedder::new();
        embedder.alternate.storage.dimension = 3;
        embedder.change_identity();
        assert!(QueryCache::new(&embedder, capacity()).is_err());
        assert_eq!(embedder.calls(), 0);
        assert_eq!(embedder.raw_calls.load(Ordering::SeqCst), 0);
    }
}
