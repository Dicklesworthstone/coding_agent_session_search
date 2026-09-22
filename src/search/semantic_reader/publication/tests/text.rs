//! Publication identity/lifecycle tests using the parent's explicitly declared
//! axis-vector fixture, NOT semantic relevance or a native-model certificate.
//! The opt-in native test below uses actual installed MiniLM weights instead.

use super::*;
use crate::search::embedder::Embedder;
use crate::search::semantic_reader::text::TextQueryProducers;
use frankensearch::{ModelCategory, SearchError};
use std::sync::atomic::AtomicBool;

struct DeclaredFixtureProducer {
    identity: EmbeddingIdentityBundleV1,
    calls: AtomicUsize,
    fail: AtomicBool,
}

impl DeclaredFixtureProducer {
    fn new(role: SemanticArtifactRole) -> Self {
        Self {
            identity: query(role).identity().clone(),
            calls: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Embedder for DeclaredFixtureProducer {
    fn embed_sync(&self, _text: &str) -> Result<Vec<f32>, SearchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            return Err(SearchError::EmbeddingFailed {
                model: "declared-publication-fixture".into(),
                source: Box::new(std::io::Error::other("injected quality failure")),
            });
        }
        // The same axis vector used by the parent's bound-vector tests. This
        // exercises only wiring and never stands in for learned inference.
        Ok(vec![1.0, 0.0, 0.0, 0.0])
    }
    fn identity(&self) -> Result<&EmbeddingIdentityBundleV1, SearchError> {
        Ok(&self.identity)
    }
    fn dimension(&self) -> usize {
        4
    }
    fn id(&self) -> &str {
        "declared-publication-fixture"
    }
    fn is_semantic(&self) -> bool {
        true
    }
    fn category(&self) -> ModelCategory {
        ModelCategory::TransformerEmbedder
    }
}

#[test]
fn selected_text_quality_only_retains_the_exact_publication_after_reader_drop() -> TestResult {
    let root = tempfile::tempdir()?;
    let m = fixture(root.path(), "gen-text-quality", 1, None, Some(B));
    let pointer = select(root.path(), &m, None);
    let reader = open(root.path(), &m);
    let producer = DeclaredFixtureProducer::new(SemanticArtifactRole::QualityVector);
    let before = snapshot(root.path());
    let batch = {
        let request =
            reader.activate_text("private text query", TextQueryProducers::Quality(&producer))?;
        assert_eq!(producer.calls(), 0);
        assert_eq!(request.selection().pointer(), &pointer);
        assert!(!format!("{request:?}").contains("private text query"));
        request.search(2, None)?
    };
    assert_eq!(producer.calls(), 1);
    assert!(Arc::ptr_eq(&batch.identity, &reader.identity));
    assert_eq!(batch.selection().manifest(), &m);
    assert_eq!(batch.selection().corpus(), &m.corpus);
    assert_eq!(first_id(&batch), 2);
    assert!(batch.batch().coverage().fast.is_none());
    let witness = reader.witness(TierKind::Quality).unwrap().clone();
    drop(reader);
    assert_eq!(batch.batch().witness(TierKind::Quality, 0), Some(&witness));
    assert_eq!(batch.selection().pointer(), &pointer);
    assert_eq!(snapshot(root.path()), before);
    Ok(())
}

#[test]
fn selected_text_progressive_stays_on_one_publication_then_refreshes_explicitly() -> TestResult {
    let root = tempfile::tempdir()?;
    let first = fixture(root.path(), "gen-text-first", 1, Some(&A[..1]), Some(B));
    let pointer = select(root.path(), &first, None);
    let mut reader = open(root.path(), &first);
    let fast = DeclaredFixtureProducer::new(SemanticArtifactRole::FastVector);
    let quality = DeclaredFixtureProducer::new(SemanticArtifactRole::QualityVector);
    let old_batch;
    {
        let request = reader.activate_text(
            "query",
            TextQueryProducers::Progressive {
                fast: &fast,
                quality: &quality,
            },
        )?;
        let mut phases = request.progressive(2, None)?;
        assert_eq!((fast.calls(), quality.calls()), (0, 0));
        let initial = phases.next().unwrap()?;
        assert_eq!(
            initial.batch().coverage().phase,
            SemanticResultPhase::Initial
        );
        assert_eq!((fast.calls(), quality.calls()), (1, 0));
        assert!(initial.batch().coverage().quality.is_none());
        let second = fixture(root.path(), "gen-text-second", 2, Some(B), Some(A));
        select(root.path(), &second, Some(&pointer));
        // Even removing the old publication's pathname cannot change the
        // already checked bytes or the provenance associated with refinement.
        let old_dir = first.generation_dir(root.path())?;
        fs::rename(&old_dir, root.path().join("retained-generation"))?;
        let before = snapshot(root.path());
        old_batch = phases.next().unwrap()?;
        assert_eq!((fast.calls(), quality.calls()), (1, 1));
        assert_eq!(old_batch.selection().pointer(), &pointer);
        assert!(Arc::ptr_eq(&initial.identity, &old_batch.identity));
        assert_eq!(
            old_batch.batch().coverage().phase,
            SemanticResultPhase::Refined
        );
        let recovered = old_batch
            .batch()
            .hits()
            .iter()
            .find(|hit| hit.document.message_id == 2)
            .unwrap();
        assert!(recovered.fast.is_none());
        assert_eq!(recovered.quality.as_ref().unwrap().tier_rank, 1);
        assert!(phases.next().is_none());
        assert!(phases.next().is_none());
        assert_eq!(snapshot(root.path()), before);
    }
    assert!(reader.refresh_current(&first.corpus, SemanticSelectionBudget::default())?);
    assert_eq!(
        reader.selection().pointer().selection_epoch,
        pointer.selection_epoch + 1
    );
    let request = reader.activate_text("query", TextQueryProducers::Fast(&fast))?;
    assert_eq!(first_id(&request.search(1, None)?), 2);
    assert_eq!(old_batch.selection().pointer(), &pointer);
    assert_eq!(
        old_batch
            .batch()
            .witness(TierKind::Quality, 0)
            .unwrap()
            .generation
            .sequence,
        1
    );
    Ok(())
}

#[test]
fn selected_text_rejects_foreign_quality_before_fast_work_and_terminates_failed_refinement()
-> TestResult {
    let root = tempfile::tempdir()?;
    let m = fixture(root.path(), "gen-text-failure", 1, Some(A), Some(B));
    let pointer = select(root.path(), &m, None);
    let reader = open(root.path(), &m);
    let fast = DeclaredFixtureProducer::new(SemanticArtifactRole::FastVector);
    let mut quality = DeclaredFixtureProducer::new(SemanticArtifactRole::QualityVector);
    quality
        .identity
        .producer
        .implementation_revision
        .push_str("-foreign");
    quality.identity.validate()?;
    assert!(matches!(
        reader.activate_text(
            "query",
            TextQueryProducers::Progressive {
                fast: &fast,
                quality: &quality
            }
        ),
        Err(SemanticReaderError::ForeignProducer(TierKind::Quality))
    ));
    assert_eq!((fast.calls(), quality.calls()), (0, 0));
    let quality = DeclaredFixtureProducer::new(SemanticArtifactRole::QualityVector);
    quality.fail.store(true, Ordering::SeqCst);
    let before = snapshot(root.path());
    let request = reader.activate_text(
        "query",
        TextQueryProducers::Progressive {
            fast: &fast,
            quality: &quality,
        },
    )?;
    let mut phases = request.progressive(1, None)?;
    let initial = phases.next().unwrap()?;
    assert!(phases.next().unwrap().is_err());
    assert!(phases.next().is_none());
    assert_eq!(initial.selection().pointer(), &pointer);
    assert_eq!(
        initial.batch().coverage().phase,
        SemanticResultPhase::Initial
    );
    quality.fail.store(false, Ordering::SeqCst);
    assert!(request.search(2, None).is_ok());
    assert_eq!(snapshot(root.path()), before);
    Ok(())
}

#[test]
fn selected_text_keeps_native_graph_policy_and_skips_zero_work_inference() -> TestResult {
    let root = tempfile::tempdir()?;
    let mut m = fixture(root.path(), "gen-text-ann", 1, Some(A), Some(B));
    for role in [
        SemanticArtifactRole::FastVector,
        SemanticArtifactRole::QualityVector,
    ] {
        attach_native_graph(root.path(), &mut m, role, 1);
    }
    let pointer = select(root.path(), &m, None);
    let reader = open(root.path(), &m).with_ann(AnnAdmissionBudget::default())?;
    let fast = DeclaredFixtureProducer::new(SemanticArtifactRole::FastVector);
    let quality = DeclaredFixtureProducer::new(SemanticArtifactRole::QualityVector);
    let request = reader.activate_text(
        "query",
        TextQueryProducers::Progressive {
            fast: &fast,
            quality: &quality,
        },
    )?;
    let before = snapshot(root.path());
    let empty = request.search_with_ann(0, None, AnnSearchPolicy::default())?;
    assert!(empty.batch().hits().is_empty());
    assert_eq!((fast.calls(), quality.calls()), (0, 0));
    let exact = request.search(2, None)?;
    assert_engine(&exact, SemanticShardEngine::Exact);
    for result in request.progressive_with_ann(2, None, AnnSearchPolicy::default())? {
        let batch = result?;
        assert_eq!(batch.selection().pointer(), &pointer);
        assert!(Arc::ptr_eq(&batch.identity, &reader.identity));
        assert_engine(&batch, SemanticShardEngine::NativeAnn);
    }
    // Abandoning a new Initial never runs its quality producer.
    let previous_quality_calls = quality.calls();
    let mut phases = request.progressive_with_ann(1, None, AnnSearchPolicy::default())?;
    phases.next().unwrap()?;
    drop(phases);
    assert_eq!(quality.calls(), previous_quality_calls);
    assert_eq!(snapshot(root.path()), before);
    Ok(())
}

#[test]
#[ignore = "requires installed native MiniLM model via CASS_TEST_EMBEDDER_MODEL; never downloads"]
fn selected_text_native_minilm_uses_actual_producer_vectors_and_published_owners() -> TestResult {
    use crate::search::fastembed_embedder::FastEmbedder;
    let directory = std::env::var_os("CASS_TEST_EMBEDDER_MODEL")
        .ok_or("set CASS_TEST_EMBEDDER_MODEL to the verified local safetensors bundle")?;
    let model = FastEmbedder::load_from_dir(Path::new(&directory))?;
    let root = tempfile::tempdir()?;
    // Keep the existing deterministic publication metadata fixture, but replace
    // its private (not yet selected) vectors and every affected witness with
    // real model output before publication. Never label fixture axes as native.
    let mut m = fixture(root.path(), "gen-text-native-model", 1, None, Some(A));
    let texts = [
        "Rust compiler ownership and borrow checking",
        "Tropical rainfall and ocean temperature",
    ];
    let corpus_digest = digest(&serde_json::to_vec(&texts)?);
    let mut identity = model.identity()?.clone();
    identity.storage.format = "fsvi-v2".into();
    identity.storage.quantization = QuantizationFormat::F16;
    identity.storage.endianness = "little-endian".into();
    let binding = FsviV2IdentityBinding::new(
        ArtifactGenerationIdentityV1::new(1, [42; 16])?,
        identity.freeze()?,
    )?;
    let path = m
        .generation_dir(root.path())?
        .join(&m.artifacts[0].relative_path);
    let mut writer = VectorIndex::create_v2(&path, binding.clone())?;
    for (position, text) in texts.iter().enumerate() {
        let mut id =
            crate::search::vector_index::parse_semantic_doc_id(&document(position as u64 + 1))
                .unwrap();
        id.content_hash = Some(Sha256::digest(text.as_bytes()).into());
        let bound = model.embed_bound_sync(text)?;
        let query = BoundQueryEmbedding::new(bound.values, bound.identity)?;
        assert!(matches!(
            query.verify_producer_conformance(&binding.frozen_identity().identity, "quality")?,
            frankensearch::core::SpaceIdentityAdmission::SameProducer
        ));
        writer.write_record(&id.to_doc_id_string(), query.vector())?;
    }
    writer.finish()?;
    let owner = ValidatedFsviBytes::open_published(&path, &binding)?;
    let witness = owner.witness().clone();
    let artifact = &mut m.artifacts[0];
    artifact.embedding_identity = binding.frozen_identity().clone();
    artifact.artifact_sha256 = hex::encode(witness.whole_image_sha256);
    artifact.size_bytes = witness.byte_len;
    artifact.covered_live_docset_sha256 = hex::encode(witness.ordered_live_docset_digest);
    artifact.generation_corpus_sha256 = corpus_digest.clone();
    artifact.covered_content_sha256 = corpus_digest.clone();
    artifact.validation.artifact_sha256 = artifact.artifact_sha256.clone();
    artifact.validation.size_bytes = artifact.size_bytes;
    artifact.validation.covered_live_docset_sha256 = artifact.covered_live_docset_sha256.clone();
    artifact.validation.generation_corpus_sha256 = corpus_digest.clone();
    artifact.validation.covered_content_sha256 = corpus_digest.clone();
    m.corpus.ordered_live_docset_sha256 = artifact.covered_live_docset_sha256.clone();
    m.corpus.content_sha256 = corpus_digest.clone();
    m.corpus.corpus_sha256 = corpus_digest;
    m.validate()?;
    drop(owner);
    let pointer = select(root.path(), &m, None);
    let reader = open(root.path(), &m);
    fs::rename(&path, root.path().join("retained-native.fsvi"))?;
    let before = snapshot(root.path());
    let batch = reader
        .activate_text(texts[0], TextQueryProducers::Quality(&model))?
        .search(2, None)?;
    assert_eq!(first_id(&batch), 1);
    assert_eq!(batch.selection().pointer(), &pointer);
    assert_eq!(batch.batch().witness(TierKind::Quality, 0), Some(&witness));
    assert_eq!(
        batch.batch().coverage().requested_topology,
        frankensearch::core::RetrievalTopology::QualityOnly
    );
    assert!(batch.batch().coverage().fast.is_none());
    assert_eq!(snapshot(root.path()), before);
    Ok(())
}
