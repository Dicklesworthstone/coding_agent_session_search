//! Daemon client integration re-exports.
//!
//! Canonical daemon abstractions now live in frankensearch:
//! - `frankensearch-core`: `DaemonClient`, `DaemonError`, `DaemonRetryConfig`
//! - `frankensearch-fusion`: `NoopDaemonClient`, `DaemonFallbackEmbedder`, `DaemonFallbackReranker`

use std::sync::Arc;

use frankensearch::core::EmbeddingIdentityBundleV1;
pub use frankensearch::{
    DaemonClient, DaemonConnectionIdentityV1, DaemonError, DaemonFallbackEmbedder,
    DaemonFallbackReranker, DaemonRetryConfig, NoopDaemonClient, PinnedDaemonVerifierV1,
};
use frankensearch::{ModelCategory, ModelTier, SearchError, SearchResult, SyncEmbed};

/// Local fallback whose advertised identity is pinned by the authenticated
/// daemon connection and whose actual native model identity is checked before
/// any locally produced vector is returned.
///
/// This preserves lazy model loading: `DaemonFallbackEmbedder::new_verified`
/// can compare the pinned identity without forcing the several-hundred-MiB
/// native model into the short-lived client. If daemon inference later fails,
/// the first local call loads the model, verifies its exact identity, and only
/// then releases the result.
pub struct IdentityCheckedLocalEmbedder {
    inner: Arc<dyn SyncEmbed>,
    expected: EmbeddingIdentityBundleV1,
}

impl IdentityCheckedLocalEmbedder {
    pub fn new(
        inner: Arc<dyn SyncEmbed>,
        expected: EmbeddingIdentityBundleV1,
    ) -> SearchResult<Self> {
        expected.validate()?;
        let expected_dimension = usize::try_from(expected.space.dimension).map_err(|_| {
            SearchError::UnverifiableRemoteSpace {
                producer: "<redacted-daemon-producer>".to_string(),
                reason: "pinned daemon dimension does not fit this client".to_string(),
            }
        })?;
        if inner.dimension() != expected_dimension {
            return Err(Self::identity_mismatch());
        }
        Ok(Self { inner, expected })
    }

    fn identity_mismatch() -> SearchError {
        SearchError::UnverifiableRemoteSpace {
            producer: "<redacted-daemon-producer>".to_string(),
            reason: "local fallback identity differs from the authenticated daemon".to_string(),
        }
    }

    fn validate_loaded_inner(&self) -> SearchResult<()> {
        let actual = self
            .inner
            .identity()
            .map_err(|_| Self::identity_mismatch())?;
        actual.validate().map_err(|_| Self::identity_mismatch())?;
        if actual != &self.expected {
            return Err(Self::identity_mismatch());
        }
        Ok(())
    }
}

impl SyncEmbed for IdentityCheckedLocalEmbedder {
    fn embed_sync(&self, text: &str) -> SearchResult<Vec<f32>> {
        let vector = self.inner.embed_sync(text)?;
        self.validate_loaded_inner()?;
        Ok(vector)
    }

    fn embed_batch_sync(&self, texts: &[&str]) -> SearchResult<Vec<Vec<f32>>> {
        let vectors = self.inner.embed_batch_sync(texts)?;
        self.validate_loaded_inner()?;
        Ok(vectors)
    }

    fn identity(&self) -> SearchResult<&EmbeddingIdentityBundleV1> {
        Ok(&self.expected)
    }

    fn dimension(&self) -> usize {
        self.inner.dimension()
    }

    fn id(&self) -> &str {
        self.inner.id()
    }

    fn model_name(&self) -> &str {
        self.inner.model_name()
    }

    fn is_ready(&self) -> bool {
        self.inner.is_ready()
    }

    fn is_semantic(&self) -> bool {
        self.inner.is_semantic()
    }

    fn category(&self) -> ModelCategory {
        self.inner.category()
    }

    fn tier(&self) -> ModelTier {
        self.inner.tier()
    }

    fn supports_mrl(&self) -> bool {
        self.inner.supports_mrl()
    }
}

/// Preserve CASS's operational index identifier around Frankensearch's
/// verified daemon embedder. Frankensearch intentionally exposes the immutable
/// logical model ID, while CASS's existing vector artifacts are keyed by the
/// operational ID (`minilm-384`). Compatibility is still established solely
/// by the delegated immutable identity bundle.
pub struct CassVerifiedDaemonEmbedder {
    inner: DaemonFallbackEmbedder,
    operational_id: String,
    model_name: String,
}

impl CassVerifiedDaemonEmbedder {
    pub fn new(
        inner: DaemonFallbackEmbedder,
        operational_id: impl Into<String>,
        model_name: impl Into<String>,
    ) -> Self {
        Self {
            inner,
            operational_id: operational_id.into(),
            model_name: model_name.into(),
        }
    }
}

impl SyncEmbed for CassVerifiedDaemonEmbedder {
    fn embed_sync(&self, text: &str) -> SearchResult<Vec<f32>> {
        self.inner.embed_sync(text)
    }

    fn embed_batch_sync(&self, texts: &[&str]) -> SearchResult<Vec<Vec<f32>>> {
        self.inner.embed_batch_sync(texts)
    }

    fn identity(&self) -> SearchResult<&EmbeddingIdentityBundleV1> {
        self.inner.identity()
    }

    fn dimension(&self) -> usize {
        self.inner.dimension()
    }

    fn id(&self) -> &str {
        &self.operational_id
    }

    fn model_name(&self) -> &str {
        &self.model_name
    }

    fn is_ready(&self) -> bool {
        self.inner.is_ready()
    }

    fn is_semantic(&self) -> bool {
        self.inner.is_semantic()
    }

    fn category(&self) -> ModelCategory {
        self.inner.category()
    }

    fn tier(&self) -> ModelTier {
        self.inner.tier()
    }

    fn supports_mrl(&self) -> bool {
        self.inner.supports_mrl()
    }
}
