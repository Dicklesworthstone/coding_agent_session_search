//! Request-local reuse of actual identity-bearing producer responses.
//! Never cache across requests, stamp expected identities onto raw vectors, or
//! replace the original producer's identity checks with the cached identity.

use std::sync::OnceLock;

use crate::search::embedder::{Embedder, EmbedderResult};
use crate::search::semantic_reader::text::TextQueryProducers;
use frankensearch::core::{EmbeddingIdentityBundleV1, IdentityBoundEmbedding, SearchError};
use frankensearch::{ModelCategory, ModelTier};

pub(super) struct CachedProducer<'request> {
    text: &'request str,
    inner: &'request dyn Embedder,
    response: OnceLock<IdentityBoundEmbedding>,
}

impl<'request> CachedProducer<'request> {
    fn new(text: &'request str, inner: &'request dyn Embedder) -> Self {
        Self { text, inner, response: OnceLock::new() }
    }
}

impl Embedder for CachedProducer<'_> {
    fn embed_sync(&self, text: &str) -> EmbedderResult<Vec<f32>> {
        Ok(self.embed_bound_sync(text)?.values)
    }

    fn embed_bound_sync(&self, text: &str) -> EmbedderResult<IdentityBoundEmbedding> {
        if text != self.text {
            return Err(SearchError::InvalidConfig {
                field: "current_semantic_query".into(),
                value: "redacted".into(),
                reason: "request-local embedding cannot serve different text".into(),
            });
        }
        if self.response.get().is_none() {
            // Called only by this request's serial retrieval loop. This adapter
            // is private and never given to a parallel inference scheduler.
            let bound = self.inner.embed_bound_sync(text)?;
            bound.validate()?;
            let _ = self.response.set(bound);
        }
        let bound = self.response.get().ok_or_else(|| SearchError::InvalidConfig {
            field: "current_semantic_query".into(), value: "redacted".into(),
            reason: "request-local embedding response was not retained".into(),
        })?;
        Ok(IdentityBoundEmbedding { values: bound.values.clone(), identity: bound.identity.clone() })
    }

    fn identity(&self) -> EmbedderResult<&EmbeddingIdentityBundleV1> {
        self.inner.identity()
    }

    fn dimension(&self) -> usize { self.inner.dimension() }
    fn id(&self) -> &str { self.inner.id() }
    fn is_semantic(&self) -> bool { self.inner.is_semantic() }
    fn category(&self) -> ModelCategory { self.inner.category() }
    fn tier(&self) -> ModelTier { self.inner.tier() }
}

pub(super) enum CachedProducers<'request> {
    Fast(CachedProducer<'request>),
    Quality(CachedProducer<'request>),
    Progressive(CachedProducer<'request>, CachedProducer<'request>),
}

impl<'request> CachedProducers<'request> {
    pub(super) fn new(text: &'request str, producers: TextQueryProducers<'request>) -> Self {
        match producers {
            TextQueryProducers::Fast(fast) => Self::Fast(CachedProducer::new(text, fast)),
            TextQueryProducers::Quality(quality) => Self::Quality(CachedProducer::new(text, quality)),
            TextQueryProducers::Progressive { fast, quality } => Self::Progressive(
                CachedProducer::new(text, fast), CachedProducer::new(text, quality),
            ),
        }
    }

    pub(super) fn producers(&self) -> TextQueryProducers<'_> {
        match self {
            Self::Fast(fast) => TextQueryProducers::Fast(fast),
            Self::Quality(quality) => TextQueryProducers::Quality(quality),
            Self::Progressive(fast, quality) => TextQueryProducers::Progressive { fast, quality },
        }
    }
}
