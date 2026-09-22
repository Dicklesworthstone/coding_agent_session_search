//! Identity for the CASS hash algorithm, not its case-sensitive delegate.
//!
//! The certificate is computed from this implementation's real output over a
//! fixed public corpus. It identifies an explicit non-semantic control; it is
//! not a learned-relevance claim or admission of a foreign producer.

use frankensearch::core::generation::GoldenVectorCertificateV1;
use frankensearch::core::{EmbeddingIdentityBundleV1, EmbeddingSpaceKindV1};

use super::{Embedder, EmbedderError, EmbedderResult, HashEmbedder};

const CONFORMANCE_TEXTS: [&str; 7] = [
    "CASS hash identity v1",
    "Case CASE case",
    "café 東京 résumé",
    "é 中 a I",
    " \t\n",
    "\0!?",
    "alpha bravo charlie delta",
];
const NORMALIZATION: &str = "l2-f32; uniform-positive-unit-vector-if-tokenless-or-exactly-cancelled-v1";

impl HashEmbedder {
    pub(super) fn bound_identity(&self) -> EmbedderResult<&EmbeddingIdentityBundleV1> {
        if let Some(identity) = self.identity.get() {
            return Ok(identity);
        }
        // The delegate supplies the actual upstream implementation revision and
        // signed-bucket rules. Never borrow its identity unchanged: CASS changes
        // tokenization and the zero-signal output, hence the mathematical space.
        let mut identity = frankensearch::Embedder::identity(&self.delegate)?.clone();
        let unicode = std::char::UNICODE_VERSION;
        let rules = format!(
            "Rust Unicode {}.{}.{} lowercase; split non-alphanumeric; retain tokens with at least 2 Unicode scalar values; join with ASCII space before delegate tokenization",
            unicode.0, unicode.1, unicode.2
        );
        let profile = identity.space.hash_control.as_mut().ok_or_else(|| {
            EmbedderError::InvalidConfig {
                field: "hash.identity".into(),
                value: self.id.clone(),
                reason: "upstream hash producer has no hash-control profile".into(),
            }
        })?;
        profile.algorithm_revision = "cass-fnv1a-v1".into();
        profile.tokenization_rules = rules.clone();
        profile.normalization_rules = NORMALIZATION.into();
        let profile_fingerprint = profile.fingerprint();
        identity.input.canonicalization = rules.clone();
        identity.input.content_selection = "one-nonempty-caller-utf8-string-v1".into();
        identity.space.logical_model_id = "cass-fnv1a-hash-control".into();
        identity.space.immutable_revision = format!(
            "cass-fnv1a-v1:unicode={}.{}.{}:dimension={}",
            unicode.0, unicode.1, unicode.2, self.dimension
        );
        identity.space.kind = EmbeddingSpaceKindV1::HashControl;
        identity.space.artifact_manifest_fingerprint = profile_fingerprint.clone();
        identity.space.tokenizer_fingerprint = profile_fingerprint.clone();
        identity.space.vocabulary_fingerprint = profile_fingerprint.clone();
        identity.space.model_config_fingerprint = profile_fingerprint.clone();
        identity.space.model_preprocessing = rules;
        identity.space.output_normalization = NORMALIZATION.into();
        identity.space.input_contract_fingerprint = identity.input.fingerprint();
        identity.producer.backend = "cass-frankensearch-hash-native".into();
        identity.producer.implementation_revision = format!(
            "cass-fnv1a-v1:delegate={}",
            identity.producer.implementation_revision
        );
        identity.producer.protocol_revision = "in-process-cass-hash-v1".into();
        identity.producer.provenance_manifest_fingerprint = profile_fingerprint;
        identity.producer.space_fingerprint = identity.space.fingerprint();
        let vectors = CONFORMANCE_TEXTS
            .iter()
            .map(|text| self.embed_sync(text))
            .collect::<EmbedderResult<Vec<_>>>()?;
        identity.producer.golden_vectors =
            GoldenVectorCertificateV1::from_exact_f32(&CONFORMANCE_TEXTS, &vectors)?;
        identity.storage.vector_normalization = NORMALIZATION.into();
        identity.validate()?;
        // Construction can race on the first call, but each contender computes
        // identical immutable bytes. No global dimension cache or leaked model
        // owner is needed; clones retain their own initialized identity.
        let _ = self.identity.set(identity);
        self.identity.get().ok_or_else(|| EmbedderError::InvalidConfig {
            field: "hash.identity".into(),
            value: self.id.clone(),
            reason: "hash identity initialization did not complete".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cass_identity_is_stable_dimension_bound_and_not_the_delegate_space() {
        for dimension in [1, 16, 256, 384] {
            let model = HashEmbedder::new(dimension);
            let identity = model.identity().unwrap();
            identity.validate().unwrap();
            assert_eq!(identity.space.kind, EmbeddingSpaceKindV1::HashControl);
            assert_eq!(identity.space.dimension as usize, dimension);
            assert_eq!(identity, HashEmbedder::new(dimension).identity().unwrap());
            assert_eq!(identity, model.clone().identity().unwrap());
            let delegate = frankensearch::Embedder::identity(&model.delegate).unwrap();
            assert_ne!(identity.space.fingerprint(), delegate.space.fingerprint());
            assert_ne!(identity.producer.fingerprint(), delegate.producer.fingerprint());
            assert_eq!(identity.input.fingerprint(), identity.space.input_contract_fingerprint);
        }
        assert_ne!(
            HashEmbedder::new(256).identity().unwrap().fingerprint(),
            HashEmbedder::new(384).identity().unwrap().fingerprint()
        );
    }

    #[test]
    fn certificate_binds_real_cass_case_unicode_and_zero_signal_behavior() {
        let model = HashEmbedder::new(16);
        let identity = model.identity().unwrap();
        let vectors = CONFORMANCE_TEXTS
            .iter()
            .map(|text| model.embed_sync(text).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            identity.producer.golden_vectors,
            GoldenVectorCertificateV1::from_exact_f32(&CONFORMANCE_TEXTS, &vectors).unwrap()
        );
        assert_eq!(model.embed_sync("Case CASE").unwrap(), model.embed_sync("case case").unwrap());
        // Single Unicode scalars are dropped even when they occupy multiple
        // UTF-8 bytes. The upstream delegate uses a different token-length law.
        assert_eq!(model.embed_sync("é 中 a I").unwrap(), model.uniform_fallback());
        assert_eq!(model.embed_sync("\0!?").unwrap(), model.uniform_fallback());
        assert_ne!(model.delegate.embed_sync("é 中 a I"), model.uniform_fallback());
        let mut wrong = vectors;
        wrong[0][0] += 1.0;
        assert_ne!(
            identity.producer.golden_vectors,
            GoldenVectorCertificateV1::from_exact_f32(&CONFORMANCE_TEXTS, &wrong).unwrap()
        );
    }

    #[test]
    fn bound_embeddings_preserve_existing_vectors_and_reject_empty_inputs() {
        let model = HashEmbedder::new(32);
        for text in CONFORMANCE_TEXTS {
            let bound = model.embed_bound_sync(text).unwrap();
            bound.validate().unwrap();
            assert_eq!(bound.values, model.embed_sync(text).unwrap());
            assert_eq!(&bound.identity, model.identity().unwrap());
        }
        let batch = model.embed_batch_bound_sync(&CONFORMANCE_TEXTS).unwrap();
        assert_eq!(batch.len(), CONFORMANCE_TEXTS.len());
        assert!(model.embed_bound_sync("").is_err());
        assert!(model.embed_batch_bound_sync(&["valid", ""]).is_err());
        assert!(model.embed_batch_bound_sync(&[]).unwrap().is_empty());
    }

    #[test]
    fn concurrent_identity_admission_shares_one_immutable_value() {
        let model = HashEmbedder::new(32);
        std::thread::scope(|scope| {
            let handles = (0..8).map(|_| scope.spawn(|| model.identity().unwrap())).collect::<Vec<_>>();
            let first = model.identity().unwrap();
            for handle in handles {
                assert!(std::ptr::eq(first, handle.join().unwrap()));
            }
        });
    }

    #[test]
    fn async_bridge_exposes_the_same_cass_identity() {
        let model = HashEmbedder::new(64);
        let expected = model.identity().unwrap().clone();
        let adapted = frankensearch::SyncEmbedderAdapter(model);
        assert_eq!(frankensearch::Embedder::identity(&adapted).unwrap(), &expected);
        assert!(!frankensearch::Embedder::is_semantic(&adapted));
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn cass_hash_queries_real_retained_fsvi_without_borrowing_upstream_identity()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::search::semantic_manifest::TierKind;
        use crate::search::vector_index::SemanticDocId;
        use crate::search::semantic_reader::{SemanticGenerationReader, SemanticShardExpectation};
        use crate::search::semantic_reader::text::TextQueryProducers;
        use frankensearch::core::generation::{ArtifactGenerationIdentityV1, QuantizationFormat};
        use frankensearch::core::{BoundQueryEmbedding, RetrievalTopology, TieredQueryEmbeddings};
        use frankensearch::index::{FsviV2IdentityBinding, ValidatedFsviBytes, VectorIndex};
        use sha2::{Digest, Sha256};

        let root = tempfile::tempdir()?;
        let model = HashEmbedder::new(32);
        let mut identity = model.identity()?.clone();
        identity.storage.format = "fsvi-v2".into();
        identity.storage.quantization = QuantizationFormat::F16;
        identity.storage.endianness = "little-endian".into();
        let binding = FsviV2IdentityBinding::new(
            ArtifactGenerationIdentityV1::new(7, [3; 16])?, identity.freeze()?,
        )?;
        let path = root.path().join("case-aware.fsvi");
        let mut writer = VectorIndex::create_v2(&path, binding.clone())?;
        for (message_id, text) in [(1, "authentication middleware"), (2, "unrelated quantum mechanics")] {
            let doc = SemanticDocId {
                message_id, chunk_idx: 0, agent_id: 1, workspace_id: 2, source_id: 3,
                role: 1, created_at_ms: 100, content_hash: Some(Sha256::digest(text.as_bytes()).into()),
            };
            writer.write_record(&doc.to_doc_id_string(), &model.embed_bound_sync(text)?.values)?;
        }
        writer.finish()?;
        let witness = ValidatedFsviBytes::open_published(&path, &binding)?.witness().clone();
        let reader = SemanticGenerationReader::open(Some(&[SemanticShardExpectation {
            path: path.clone(), binding, witness: witness.clone(),
        }]), None)?;
        let before = std::fs::read(&path)?;
        let batch = reader.activate_text("AUTHENTICATION middleware", TextQueryProducers::Fast(&model))?.search(1, None)?;
        assert_eq!(batch.hits()[0].document.message_id, 1);
        assert_eq!(batch.coverage().requested_topology, RetrievalTopology::HashControl);
        assert_eq!(batch.witness(TierKind::Fast, 0), Some(&witness));
        // Equal dimensions and the same display family are not compatibility.
        let foreign = BoundQueryEmbedding::new(
            model.delegate.embed_sync("AUTHENTICATION middleware"),
            frankensearch::Embedder::identity(&model.delegate)?.clone(),
        )?;
        assert!(reader.activate(&TieredQueryEmbeddings::fast_only(foreign)).is_err());
        assert_eq!(std::fs::read(path)?, before);
        Ok(())
    }
}
