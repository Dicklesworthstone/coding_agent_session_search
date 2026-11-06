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

/// Local fallback whose complete declared identity must match the authenticated
/// daemon before composition, and whose actual producer is checked again before
/// any locally produced vector is returned.
///
/// This preserves lazy model loading: `DaemonFallbackEmbedder::new_verified`
/// can compare the local frozen identity without forcing the several-hundred-MiB
/// native model into the short-lived client. The daemon cannot supply the local
/// producer's expected identity. If daemon inference later fails,
/// the first local call loads the model, verifies its exact identity, and only
/// then releases the result.
pub struct IdentityCheckedLocalEmbedder {
    inner: Arc<dyn SyncEmbed>,
    expected: EmbeddingIdentityBundleV1,
    dimension: usize,
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
        let declared = inner.identity().map_err(|_| Self::identity_mismatch())?;
        declared.validate().map_err(|_| Self::identity_mismatch())?;
        if declared != &expected {
            return Err(Self::identity_mismatch());
        }
        Ok(Self {
            inner,
            expected,
            dimension: expected_dimension,
        })
    }

    fn identity_mismatch() -> SearchError {
        SearchError::UnverifiableRemoteSpace {
            producer: "<redacted-daemon-producer>".to_string(),
            reason: "local fallback identity differs from the authenticated daemon".to_string(),
        }
    }

    fn invalid_output() -> SearchError {
        // Do not include input text, model paths, or raw coordinates in errors.
        SearchError::UnverifiableRemoteSpace {
            producer: "<redacted-daemon-producer>".to_string(),
            reason: "local fallback must return one finite, nonzero vector of the pinned dimension per input"
                .to_string(),
        }
    }

    fn validate_vector(&self, vector: &[f32]) -> SearchResult<()> {
        if vector.len() != self.dimension
            || vector.iter().any(|value| !value.is_finite())
            || vector.iter().all(|value| *value == 0.0)
        {
            return Err(Self::invalid_output());
        }
        Ok(())
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
        self.validate_vector(&vector)?;
        Ok(vector)
    }

    fn embed_batch_sync(&self, texts: &[&str]) -> SearchResult<Vec<Vec<f32>>> {
        // No output needs admission, and an empty request must not load a model.
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let vectors = self.inner.embed_batch_sync(texts)?;
        self.validate_loaded_inner()?;
        if vectors.len() != texts.len() {
            return Err(Self::invalid_output());
        }
        for vector in &vectors {
            self.validate_vector(vector)?;
        }
        Ok(vectors)
    }

    fn identity(&self) -> SearchResult<&EmbeddingIdentityBundleV1> {
        Ok(&self.expected)
    }

    fn dimension(&self) -> usize {
        self.dimension
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

#[cfg(test)]
mod tests {
    use super::*;
    use frankensearch::core::generation::QuantizationFormat;
    use frankensearch::core::{
        EMBEDDING_INPUT_CONTRACT_SCHEMA_V1, EMBEDDING_PRODUCER_ATTESTATION_SCHEMA_V1,
        EMBEDDING_SPACE_IDENTITY_SCHEMA_V1, EmbeddingArtifactIdentityV1, EmbeddingInputContractV1,
        EmbeddingProducerAttestationV1, EmbeddingSpaceIdentityV1, EmbeddingSpaceKindV1,
        GoldenVectorCertificateV1, VECTOR_STORAGE_IDENTITY_SCHEMA_V1, VectorStorageIdentityV1,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Synthetic identity and vectors exercise the admission boundary, not
    // model accuracy, native inference, or daemon authentication.
    fn identity() -> EmbeddingIdentityBundleV1 {
        let input = EmbeddingInputContractV1 {
            schema_version: EMBEDDING_INPUT_CONTRACT_SCHEMA_V1,
            canonicalization: "test-canonical-v1".into(),
            content_selection: "test-content-v1".into(),
            chunking: "test-chunks-v1".into(),
            query_instruction: "query:".into(),
            document_instruction: "document:".into(),
            doc_id_semantics: "test-passage-id-v1".into(),
        };
        let space = EmbeddingSpaceIdentityV1 {
            schema_version: EMBEDDING_SPACE_IDENTITY_SCHEMA_V1,
            logical_model_id: "fallback-admission-fixture".into(),
            immutable_revision: "fixture-v1".into(),
            kind: EmbeddingSpaceKindV1::Semantic,
            artifact_manifest_fingerprint: "a".repeat(64),
            artifacts: vec![
                EmbeddingArtifactIdentityV1 {
                    role: "tokenizer".into(),
                    sha256: "b".repeat(64),
                    size: 9,
                },
                EmbeddingArtifactIdentityV1 {
                    role: "weights".into(),
                    sha256: "c".repeat(64),
                    size: 7,
                },
            ],
            tokenizer_fingerprint: "d".repeat(64),
            vocabulary_fingerprint: "e".repeat(64),
            model_config_fingerprint: "f".repeat(64),
            model_preprocessing: "unicode=nfc".into(),
            sequence_policy: "truncate=tail;max_tokens=512;padding=none".into(),
            query_instruction: "query:".into(),
            document_instruction: "document:".into(),
            pooling: "mean-mask-aware".into(),
            output_normalization: "l2-f32-v1".into(),
            dimension: 4,
            input_contract_fingerprint: input.fingerprint(),
            hash_control: None,
            projection: None,
        };
        let producer = EmbeddingProducerAttestationV1 {
            schema_version: EMBEDDING_PRODUCER_ATTESTATION_SCHEMA_V1,
            backend: "fallback-test-fixture".into(),
            implementation_revision: "fixture-v1".into(),
            protocol_revision: "in-process-v1".into(),
            numeric_profile: "test-f32-v1".into(),
            provenance_manifest_fingerprint: "1".repeat(64),
            space_fingerprint: space.fingerprint(),
            golden_vectors: GoldenVectorCertificateV1 {
                corpus_sha256: "2".repeat(64),
                vectors_sha256: "3".repeat(64),
                vector_count: 2,
                dimension: 4,
            },
        };
        EmbeddingIdentityBundleV1 {
            space,
            producer,
            input,
            storage: VectorStorageIdentityV1 {
                schema_version: VECTOR_STORAGE_IDENTITY_SCHEMA_V1,
                format: "fsvi-v2".into(),
                quantization: QuantizationFormat::F16,
                endianness: "little-endian".into(),
                vector_normalization: "l2-f32-v1".into(),
                dimension: 4,
            },
        }
    }

    struct ResponseEmbedder {
        identity: EmbeddingIdentityBundleV1,
        vector: Vec<f32>,
        batch: Vec<Vec<f32>>,
        calls: AtomicUsize,
        fail: bool,
    }

    impl SyncEmbed for ResponseEmbedder {
        fn embed_sync(&self, _text: &str) -> SearchResult<Vec<f32>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(SearchError::InvalidConfig {
                    field: "synthetic-backend".into(),
                    value: "test".into(),
                    reason: "test failure".into(),
                });
            }
            Ok(self.vector.clone())
        }

        fn embed_batch_sync(&self, _texts: &[&str]) -> SearchResult<Vec<Vec<f32>>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.batch.clone())
        }

        fn identity(&self) -> SearchResult<&EmbeddingIdentityBundleV1> {
            Ok(&self.identity)
        }

        fn dimension(&self) -> usize {
            4
        }

        fn id(&self) -> &str {
            "synthetic-fallback"
        }

        fn is_semantic(&self) -> bool {
            true
        }

        fn category(&self) -> ModelCategory {
            ModelCategory::HashEmbedder
        }
    }

    fn response(vector: Vec<f32>, batch: Vec<Vec<f32>>) -> Arc<ResponseEmbedder> {
        Arc::new(ResponseEmbedder {
            identity: identity(),
            vector,
            batch,
            calls: AtomicUsize::new(0),
            fail: false,
        })
    }

    fn checked(inner: Arc<ResponseEmbedder>) -> IdentityCheckedLocalEmbedder {
        IdentityCheckedLocalEmbedder::new(inner, identity()).expect("valid fixture identity")
    }

    #[test]
    fn fallback_admission_stays_lazy_and_empty_batches_do_not_load_a_model() {
        let inner = response(vec![1.0, 0.0, 0.0, 0.0], Vec::new());
        let fallback = checked(Arc::clone(&inner));
        assert_eq!(fallback.dimension(), 4);
        assert_eq!(fallback.identity().unwrap(), &identity());
        assert!(fallback.embed_batch_sync(&[]).unwrap().is_empty());
        assert_eq!(inner.calls.load(Ordering::SeqCst), 0);
        assert_eq!(fallback.embed_sync("query").unwrap(), inner.vector);
        assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn fallback_rejects_wrong_dimensions_nonfinite_and_zero_vectors() {
        for vector in [
            Vec::new(),
            vec![1.0; 3],
            vec![1.0; 5],
            vec![0.0, -0.0, 0.0, 0.0],
            vec![f32::NAN, 1.0, 0.0, 0.0],
            vec![f32::INFINITY, 1.0, 0.0, 0.0],
            vec![f32::NEG_INFINITY, 1.0, 0.0, 0.0],
        ] {
            let inner = response(vector, Vec::new());
            let fallback = checked(Arc::clone(&inner));
            let error = fallback.embed_sync("private query").unwrap_err();
            assert!(matches!(error, SearchError::UnverifiableRemoteSpace { .. }));
            assert!(!error.to_string().contains("private query"));
            assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn fallback_batch_admission_is_all_or_nothing_and_preserves_order() {
        let first = vec![1.0, 0.0, 0.0, 0.0];
        let second = vec![0.0, -1.0, 0.0, 0.0];
        let expected = vec![first.clone(), second.clone()];
        let fallback = checked(response(first.clone(), expected.clone()));
        assert_eq!(fallback.embed_batch_sync(&["a", "b"]).unwrap(), expected);
        for batch in [
            Vec::new(),
            vec![first.clone()],
            vec![first.clone(), second.clone(), first.clone()],
            vec![first.clone(), vec![1.0; 3]],
            vec![first.clone(), vec![f32::NAN; 4]],
            vec![first.clone(), vec![0.0; 4]],
        ] {
            let fallback = checked(response(first.clone(), batch));
            assert!(matches!(
                fallback.embed_batch_sync(&["private-a", "private-b"]),
                Err(SearchError::UnverifiableRemoteSpace { .. })
            ));
        }
    }

    #[test]
    fn fallback_rejects_same_dimension_producer_mismatch_before_composition() {
        let mut inner = response(vec![1.0, 0.0, 0.0, 0.0], Vec::new());
        Arc::get_mut(&mut inner)
            .unwrap()
            .identity
            .producer
            .implementation_revision = "different-producer".into();
        assert!(matches!(
            IdentityCheckedLocalEmbedder::new(inner, identity()),
            Err(SearchError::UnverifiableRemoteSpace { .. })
        ));
    }

    #[test]
    fn real_hash_identity_is_admitted_only_for_the_same_producer() -> SearchResult<()> {
        let local: Arc<dyn SyncEmbed> =
            Arc::new(crate::search::hash_embedder::HashEmbedder::new(384));
        let expected = local.identity()?.clone();
        let checked = IdentityCheckedLocalEmbedder::new(Arc::clone(&local), expected.clone())?;
        assert_eq!(checked.identity()?, &expected);
        assert_eq!(
            checked.embed_sync("real hash producer admission")?,
            local.embed_sync("real hash producer admission")?
        );
        // CASS's input contract differs from the upstream hash producer even
        // when both expose the same width. Neither ID nor width is authority.
        let upstream =
            frankensearch::HashEmbedder::new(384, frankensearch::HashAlgorithm::FnvModular);
        let other = frankensearch::Embedder::identity(&upstream)?.clone();
        assert_eq!(expected.space.dimension, other.space.dimension);
        assert_ne!(expected, other);
        assert!(matches!(
            IdentityCheckedLocalEmbedder::new(local, other),
            Err(SearchError::UnverifiableRemoteSpace { .. })
        ));
        Ok(())
    }

    #[test]
    fn native_daemon_composition_checks_both_frozen_profiles_before_inference() -> SearchResult<()>
    {
        use crate::search::fastembed_embedder::{FastEmbedder, LazyFastEmbedder};

        let temp = tempfile::tempdir().expect("tempdir");
        for (selected, other) in [
            ("minilm", "multilingual-minilm"),
            ("multilingual-minilm", "minilm"),
        ] {
            let local: Arc<dyn SyncEmbed> = Arc::new(LazyFastEmbedder::new(temp.path(), selected)?);
            let expected = FastEmbedder::declared_identity_for(selected)?;
            let wrong_space = FastEmbedder::declared_identity_for(other)?;
            assert_eq!(expected.space.dimension, wrong_space.space.dimension);
            assert!(matches!(
                IdentityCheckedLocalEmbedder::new(Arc::clone(&local), wrong_space),
                Err(SearchError::UnverifiableRemoteSpace { .. })
            ));
            // Admission succeeds with no model files. Only a later fallback
            // inference would initialize and verify the real installation.
            let checked = IdentityCheckedLocalEmbedder::new(local, expected.clone())?;
            assert_eq!(checked.identity()?, &expected);
        }
        assert_eq!(
            std::fs::read_dir(temp.path())
                .expect("untouched directory")
                .count(),
            0
        );
        Ok(())
    }

    #[test]
    fn native_daemon_composition_rejects_same_model_with_a_different_numeric_producer()
    -> SearchResult<()> {
        use crate::search::fastembed_embedder::{FastEmbedder, LazyFastEmbedder};
        use frankensearch::embed::model_manifest::ModelArtifactManifestV1;

        let temp = tempfile::tempdir().expect("tempdir");
        let local: Arc<dyn SyncEmbed> = Arc::new(LazyFastEmbedder::new(temp.path(), "minilm")?);
        let expected = FastEmbedder::declared_identity_for("minilm")?;
        let f32_producer = ModelArtifactManifestV1::minilm_native_frankentorch_f32()?
            .declared_identity_bundle(QuantizationFormat::F32, "in-memory-f32-v1")?;
        assert_eq!(
            expected.space.logical_model_id,
            f32_producer.space.logical_model_id
        );
        assert_eq!(expected.space.dimension, f32_producer.space.dimension);
        assert_ne!(expected.producer, f32_producer.producer);
        assert!(matches!(
            IdentityCheckedLocalEmbedder::new(local, f32_producer),
            Err(SearchError::UnverifiableRemoteSpace { .. })
        ));
        Ok(())
    }

    #[test]
    fn fallback_rechecks_producer_identity_after_inference() -> SearchResult<()> {
        struct DriftingProducer {
            before: EmbeddingIdentityBundleV1,
            after: EmbeddingIdentityBundleV1,
            calls: AtomicUsize,
        }
        impl SyncEmbed for DriftingProducer {
            fn embed_sync(&self, _: &str) -> SearchResult<Vec<f32>> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(vec![1.0, 0.0, 0.0, 0.0])
            }

            fn identity(&self) -> SearchResult<&EmbeddingIdentityBundleV1> {
                Ok(if self.calls.load(Ordering::SeqCst) == 0 {
                    &self.before
                } else {
                    &self.after
                })
            }

            fn dimension(&self) -> usize {
                4
            }
            fn id(&self) -> &str {
                "synthetic-drifting-producer"
            }
            fn is_semantic(&self) -> bool {
                true
            }
            fn category(&self) -> ModelCategory {
                ModelCategory::TransformerEmbedder
            }
        }
        let before = identity();
        let mut after = before.clone();
        after.producer.implementation_revision = "changed-after-admission".into();
        after.validate()?;
        let local = Arc::new(DriftingProducer {
            before: before.clone(),
            after,
            calls: AtomicUsize::new(0),
        });
        let checked = IdentityCheckedLocalEmbedder::new(local.clone(), before)?;
        assert_eq!(local.calls.load(Ordering::SeqCst), 0);
        assert!(matches!(
            checked.embed_sync("private query"),
            Err(SearchError::UnverifiableRemoteSpace { .. })
        ));
        assert_eq!(local.calls.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[test]
    fn fallback_propagates_backend_failure_without_retry() {
        let mut inner = response(vec![1.0, 0.0, 0.0, 0.0], Vec::new());
        Arc::get_mut(&mut inner).unwrap().fail = true;
        let fallback = checked(Arc::clone(&inner));
        assert!(matches!(
            fallback.embed_sync("query"),
            Err(SearchError::InvalidConfig { .. })
        ));
        assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
    }
}
