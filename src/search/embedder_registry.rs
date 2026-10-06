//! Explicit provider selection over the unchanged local model registry.
//!
//! The static registry describes only installed local models. Dynamic external
//! identities must never be represented by a static MiniLM record. Only an
//! explicit `Some("external")` passed to the factory consults endpoint consent;
//! defaults, availability discovery and model installation remain local.

use std::path::Path;
use std::sync::Arc;

// Preserve the local registry and all its regression tests byte-for-byte in a
// child module. These aliases retain its existing sibling imports.
use super::external_embedder::{
    CancelCheck, EXTERNAL_EMBEDDER, ExternalEmbedder, ExternalEmbeddingConfig,
};
use super::{embedder, fastembed_embedder, hash_embedder};
use embedder::{Embedder, EmbedderInfo, EmbedderResult};

#[path = "embedder_registry/local.rs"]
mod local;

pub use local::{
    BAKEOFF_ELIGIBILITY_CUTOFF, DEFAULT_EMBEDDER, EMBEDDERS, EmbedderRegistry, HASH_EMBEDDER,
    REQUIRED_NATIVE_MODEL_FILES, RegisteredEmbedder,
};

fn selects_external(name: Option<&str>) -> bool {
    name.is_some_and(|name| name.trim().eq_ignore_ascii_case(EXTERNAL_EMBEDDER))
}

/// Load the explicitly selected provider. Missing local assets and external
/// failures are errors, never reasons to switch to another vector space.
pub fn get_embedder(data_dir: &Path, name: Option<&str>) -> EmbedderResult<Arc<dyn Embedder>> {
    select_embedder(data_dir, name, || {
        ExternalEmbedder::from_env().map(|provider| Arc::new(provider) as Arc<dyn Embedder>)
    })
}

/// Cancellation-aware counterpart for backfill/job owners. The external
/// constructor completes its public-input preflight before returning a provider.
/// A blocking in-flight request is bounded by the configured HTTP deadline.
pub fn get_embedder_with_cancel(
    data_dir: &Path,
    name: Option<&str>,
    cancelled: CancelCheck,
) -> EmbedderResult<Arc<dyn Embedder>> {
    select_embedder(data_dir, name, || {
        ExternalEmbedder::from_env_with_cancel(cancelled)
            .map(|provider| Arc::new(provider) as Arc<dyn Embedder>)
    })
}

fn select_embedder(
    data_dir: &Path,
    name: Option<&str>,
    external: impl FnOnce() -> EmbedderResult<Arc<dyn Embedder>>,
) -> EmbedderResult<Arc<dyn Embedder>> {
    if selects_external(name) {
        external()
    } else {
        local::get_embedder(data_dir, name)
    }
}

/// Metadata inspection never constructs an HTTP client or sends probe text.
/// External metadata exists only with explicit selection and valid consented
/// configuration; it carries the dynamic external identity and dimension.
pub fn get_embedder_info(data_dir: &Path, name: Option<&str>) -> Option<EmbedderInfo> {
    if selects_external(name) {
        let config = ExternalEmbeddingConfig::from_env().ok()??;
        Some(EmbedderInfo {
            id: config.identity(),
            dimension: config.dimension(),
            is_semantic: true,
        })
    } else {
        local::get_embedder_info(data_dir, name)
    }
}

#[cfg(test)]
mod tests {
    use super::embedder::EmbedderError;
    use super::*;

    #[test]
    fn external_selection_requires_the_explicit_name() {
        for name in [
            None,
            Some("minilm"),
            Some("hash"),
            Some("default"),
            Some("auto"),
        ] {
            assert!(!selects_external(name), "{name:?}");
        }
        for name in ["external", " EXTERNAL "] {
            assert!(selects_external(Some(name)));
        }
        // An artifact identity is not itself consent to contact a server.
        let identity = format!("external-v1-384-{}", "a".repeat(64));
        assert!(!selects_external(Some(&identity)));
    }

    #[test]
    fn local_factory_never_consults_the_external_loader() {
        let tmp = tempfile::tempdir().unwrap();
        let hash = select_embedder(tmp.path(), Some("hash"), || {
            panic!("local selection must not read external configuration or send text")
        })
        .unwrap();
        assert_eq!(hash.id(), "fnv1a-384");
        assert_eq!(hash.embed_sync("private session text").unwrap().len(), 384);
        for name in [None, Some("minilm"), Some("unknown")] {
            let result = select_embedder(tmp.path(), name, || {
                panic!("a missing local model must not fall back to an external server")
            });
            assert!(result.is_err());
        }
    }

    #[test]
    fn external_consent_failure_never_substitutes_a_local_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let mut calls = 0;
        let result = select_embedder(tmp.path(), Some("external"), || {
            calls += 1;
            Err(EmbedderError::EmbedderUnavailable {
                model: EXTERNAL_EMBEDDER.into(),
                reason: "external_disabled: explicit consent is absent".into(),
            })
        });
        let error = match result {
            Ok(_) => panic!("consent failure unexpectedly selected a provider"),
            Err(error) => error,
        };
        assert_eq!(calls, 1);
        assert!(error.to_string().contains("external_disabled"));
    }
}
