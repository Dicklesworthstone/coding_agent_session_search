//! Real hash inference and persisted FSVI v2 retrieval. Hash controls remain
//! controls: these tests do not qualify native MiniLM semantics or performance.
#![cfg(any(target_os = "linux", target_os = "android"))]

use super::*;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};

use frankensearch::core::IdentityBoundEmbedding;
use frankensearch::core::generation::QuantizationFormat;
use frankensearch::index::VectorIndex;
use frankensearch::{HashAlgorithm, HashEmbedder, ModelCategory};
use sha2::{Digest, Sha256};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Fault injection surrounds real production hash inference. It never supplies
/// fixture vectors or a successful semantic attestation.
struct ObservedHash {
    hash: HashEmbedder,
    identity: EmbeddingIdentityBundleV1,
    changed: EmbeddingIdentityBundleV1,
    identities: AtomicUsize,
    calls: AtomicUsize,
    drift: AtomicBool,
    drift_during_call: AtomicBool,
    foreign_response: AtomicBool,
    fail: AtomicBool,
}

impl ObservedHash {
    fn new(algorithm: HashAlgorithm) -> Self {
        let hash = HashEmbedder::new(16, algorithm);
        let identity = frankensearch::Embedder::identity(&hash).unwrap().clone();
        let mut changed = identity.clone();
        changed
            .producer
            .implementation_revision
            .push_str("-changed");
        changed.validate().unwrap();
        Self {
            hash,
            identity,
            changed,
            identities: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
            drift: AtomicBool::new(false),
            drift_during_call: AtomicBool::new(false),
            foreign_response: AtomicBool::new(false),
            fail: AtomicBool::new(false),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(AtomicOrdering::SeqCst)
    }
}

impl Embedder for ObservedHash {
    fn embed_sync(&self, text: &str) -> Result<Vec<f32>, SearchError> {
        self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        if self.fail.load(AtomicOrdering::SeqCst) {
            return Err(SearchError::EmbeddingFailed {
                model: "injected-hash-failure".into(),
                source: Box::new(std::io::Error::other("injected inference failure")),
            });
        }
        let values = self.hash.embed_sync(text);
        if self.drift_during_call.load(AtomicOrdering::SeqCst) {
            self.drift.store(true, AtomicOrdering::SeqCst);
        }
        Ok(values)
    }

    fn identity(&self) -> Result<&EmbeddingIdentityBundleV1, SearchError> {
        self.identities.fetch_add(1, AtomicOrdering::SeqCst);
        Ok(if self.drift.load(AtomicOrdering::SeqCst) {
            &self.changed
        } else {
            &self.identity
        })
    }

    fn embed_bound_sync(&self, text: &str) -> Result<IdentityBoundEmbedding, SearchError> {
        let values = self.embed_sync(text)?;
        let identity = if self.foreign_response.load(AtomicOrdering::SeqCst) {
            self.changed.clone()
        } else {
            self.identity()?.clone()
        };
        let bound = IdentityBoundEmbedding { values, identity };
        bound.validate()?;
        Ok(bound)
    }

    fn dimension(&self) -> usize {
        16
    }
    fn id(&self) -> &str {
        "observed-real-hash"
    }
    fn is_semantic(&self) -> bool {
        false
    }
    fn category(&self) -> ModelCategory {
        ModelCategory::HashEmbedder
    }
}

fn shard(
    dir: &Path,
    name: &str,
    producer: &ObservedHash,
    rows: &[(u64, u32, &str)],
) -> SemanticShardExpectation {
    let mut identity = producer.identity.clone();
    identity.storage.format = "fsvi-v2".into();
    identity.storage.quantization = QuantizationFormat::F16;
    identity.storage.endianness = "little-endian".into();
    let binding = FsviV2IdentityBinding::new(
        ArtifactGenerationIdentityV1::new(42, [7; 16]).unwrap(),
        identity.freeze().unwrap(),
    )
    .unwrap();
    let path = dir.join(name);
    let mut writer = VectorIndex::create_v2(&path, binding.clone()).unwrap();
    for (id, source, text) in rows {
        let document = SemanticDocId {
            message_id: *id,
            chunk_idx: 0,
            agent_id: 1,
            workspace_id: 2,
            source_id: *source,
            role: ROLE_USER,
            created_at_ms: 100,
            content_hash: Some(Sha256::digest(text.as_bytes()).into()),
        };
        writer
            .write_record(
                &document.to_doc_id_string(),
                &producer.hash.embed_sync(text),
            )
            .unwrap();
    }
    writer.finish().unwrap();
    let witness = ValidatedFsviBytes::open_published(&path, &binding)
        .unwrap()
        .witness()
        .clone();
    SemanticShardExpectation {
        path,
        binding,
        witness,
    }
}

fn ids(batch: &SemanticSearchBatch) -> Vec<u64> {
    batch
        .hits()
        .iter()
        .map(|hit| hit.document.message_id)
        .collect()
}

fn signature(batch: &SemanticSearchBatch) -> Vec<(String, u64)> {
    batch
        .hits()
        .iter()
        .map(|hit| (hit.doc_id.clone(), hit.ranking_score.to_bits()))
        .collect()
}

#[test]
fn text_quality_only_uses_real_inference_and_its_own_shards() -> TestResult {
    let temp = tempfile::tempdir()?;
    let quality = ObservedHash::new(HashAlgorithm::JLProjection { seed: 9 });
    let shards = [
        shard(
            temp.path(),
            "q-a.fsvi",
            &quality,
            &[(1, 3, "unrelated storage")],
        ),
        shard(
            temp.path(),
            "q-b.fsvi",
            &quality,
            &[(2, 3, "authentication middleware")],
        ),
    ];
    let reader = SemanticGenerationReader::open(None, Some(&shards))?;
    let before = fs::read(&shards[1].path)?;
    let request = reader.activate_text(
        "authentication middleware",
        TextQueryProducers::Quality(&quality),
    )?;
    assert_eq!(quality.calls(), 0, "activation is not inference");
    let batch = request.search(2, None)?;
    assert_eq!(quality.calls(), 1);
    assert_eq!(ids(&batch)[0], 2);
    assert_eq!(
        batch.coverage().requested_topology,
        RetrievalTopology::HashControl
    );
    assert!(batch.coverage().fast.is_none());
    assert_eq!(
        batch.coverage().quality.as_ref().unwrap().selected_shards,
        2
    );
    let queries = TieredQueryEmbeddings::quality_only(BoundQueryEmbedding::new(
        quality.hash.embed_sync("authentication middleware"),
        quality.identity.clone(),
    )?);
    assert_eq!(
        signature(&batch),
        signature(&reader.activate(&queries)?.search(2, None)?)
    );
    assert_eq!(fs::read(&shards[1].path)?, before);
    Ok(())
}

#[test]
fn text_progressive_is_lazy_and_quality_recovers_a_fast_miss() -> TestResult {
    let temp = tempfile::tempdir()?;
    let fast = ObservedHash::new(HashAlgorithm::FnvModular);
    let quality = ObservedHash::new(HashAlgorithm::JLProjection { seed: 9 });
    let f = [shard(
        temp.path(),
        "fast.fsvi",
        &fast,
        &[(1, 3, "filesystem change")],
    )];
    let q = [shard(
        temp.path(),
        "quality.fsvi",
        &quality,
        &[(2, 3, "authentication middleware")],
    )];
    let reader = SemanticGenerationReader::open(Some(&f), Some(&q))?;
    let request = reader.activate_text(
        "authentication middleware",
        TextQueryProducers::Progressive {
            fast: &fast,
            quality: &quality,
        },
    )?;
    let mut stream = request.progressive(2, None)?;
    assert_eq!((fast.calls(), quality.calls()), (0, 0));
    let first = stream.next().unwrap()?;
    assert_eq!(ids(&first), [1]);
    assert_eq!(first.coverage().phase, SemanticResultPhase::Initial);
    assert!(first.coverage().quality.is_none());
    assert_eq!((fast.calls(), quality.calls()), (1, 0));
    let refined = stream.next().unwrap()?;
    assert!(
        ids(&refined).contains(&2),
        "quality must retrieve outside the fast pool"
    );
    assert_eq!(refined.coverage().phase, SemanticResultPhase::Refined);
    assert_eq!(
        refined.score_kind(),
        SemanticScoreKind::ReciprocalRankFusion
    );
    assert_eq!((fast.calls(), quality.calls()), (1, 1));
    assert!(stream.next().is_none());
    assert!(stream.next().is_none());
    assert_eq!(signature(&refined), signature(&request.search(2, None)?));
    let mut dropped = request.progressive(2, None)?;
    dropped.next().unwrap()?;
    drop(dropped);
    assert_eq!((fast.calls(), quality.calls()), (3, 2));
    Ok(())
}

#[test]
fn text_bad_quality_producer_is_rejected_before_fast_inference() -> TestResult {
    let temp = tempfile::tempdir()?;
    let fast = ObservedHash::new(HashAlgorithm::FnvModular);
    let quality = ObservedHash::new(HashAlgorithm::JLProjection { seed: 9 });
    let other = ObservedHash::new(HashAlgorithm::JLProjection { seed: 10 });
    let f = [shard(temp.path(), "fast.fsvi", &fast, &[(1, 3, "alpha")])];
    let q = [shard(
        temp.path(),
        "quality.fsvi",
        &quality,
        &[(2, 3, "beta")],
    )];
    let reader = SemanticGenerationReader::open(Some(&f), Some(&q))?;
    assert!(
        reader
            .activate_text(
                "alpha",
                TextQueryProducers::Progressive {
                    fast: &fast,
                    quality: &other
                }
            )
            .is_err()
    );
    assert_eq!((fast.calls(), quality.calls(), other.calls()), (0, 0, 0));
    quality.drift.store(true, AtomicOrdering::SeqCst);
    assert!(matches!(
        reader.activate_text("alpha", TextQueryProducers::Quality(&quality)),
        Err(SemanticReaderError::ForeignProducer(TierKind::Quality))
    ));
    assert_eq!(
        quality.calls(),
        0,
        "matching golden vectors cannot authorize a foreign producer"
    );
    Ok(())
}

#[test]
fn text_invalid_input_and_missing_tier_precede_producer_inspection() -> TestResult {
    let temp = tempfile::tempdir()?;
    let hash = ObservedHash::new(HashAlgorithm::FnvModular);
    let f = [shard(temp.path(), "fast.fsvi", &hash, &[(1, 3, "alpha")])];
    let reader = SemanticGenerationReader::open(Some(&f), None)?;
    for input in ["".to_owned(), " \n\t".to_owned(), "é".repeat(2049)] {
        assert!(
            reader
                .activate_text(&input, TextQueryProducers::Fast(&hash))
                .is_err()
        );
    }
    assert!(matches!(
        reader.activate_text(
            "alpha",
            TextQueryProducers::Progressive {
                fast: &hash,
                quality: &hash
            }
        ),
        Err(SemanticReaderError::MissingTier(TierKind::Quality))
    ));
    assert_eq!(hash.identities.load(AtomicOrdering::SeqCst), 0);
    assert_eq!(hash.calls(), 0);
    let boundary = "é".repeat(2048);
    reader.activate_text(&boundary, TextQueryProducers::Fast(&hash))?;
    Ok(())
}

#[test]
fn text_empty_and_zero_k_skip_inference_without_weakening_identity() -> TestResult {
    let temp = tempfile::tempdir()?;
    let hash = ObservedHash::new(HashAlgorithm::FnvModular);
    let empty = [shard(temp.path(), "empty.fsvi", &hash, &[])];
    let nonempty = [shard(temp.path(), "full.fsvi", &hash, &[(1, 3, "alpha")])];
    for (f, k) in [(&empty, usize::MAX), (&nonempty, 0)] {
        let reader = SemanticGenerationReader::open(Some(f), None)?;
        let request = reader.activate_text("alpha", TextQueryProducers::Fast(&hash))?;
        let batch = request.search(k, None)?;
        assert!(batch.hits().is_empty());
        assert!(
            batch
                .execution()
                .iter()
                .all(|report| report.engine == SemanticShardEngine::Skipped)
        );
        assert_eq!(hash.calls(), 0);
        hash.drift.store(true, AtomicOrdering::SeqCst);
        assert!(request.search(k, None).is_err());
        hash.drift.store(false, AtomicOrdering::SeqCst);
    }
    Ok(())
}

#[test]
fn text_mid_call_and_response_identity_drift_never_publish_results() -> TestResult {
    let temp = tempfile::tempdir()?;
    let hash = ObservedHash::new(HashAlgorithm::FnvModular);
    let f = [shard(temp.path(), "fast.fsvi", &hash, &[(1, 3, "alpha")])];
    let reader = SemanticGenerationReader::open(Some(&f), None)?;
    let request = reader.activate_text("alpha", TextQueryProducers::Fast(&hash))?;
    hash.foreign_response.store(true, AtomicOrdering::SeqCst);
    assert!(request.search(1, None).is_err());
    hash.foreign_response.store(false, AtomicOrdering::SeqCst);
    hash.drift_during_call.store(true, AtomicOrdering::SeqCst);
    assert!(request.search(1, None).is_err());
    assert_eq!(hash.calls(), 2);
    assert!(request.search(1, None).is_err());
    assert_eq!(
        hash.calls(),
        2,
        "drift must now reject before further inference"
    );
    Ok(())
}

#[test]
fn text_quality_failure_and_between_phase_drift_fuse_the_iterator() -> TestResult {
    let temp = tempfile::tempdir()?;
    let fast = ObservedHash::new(HashAlgorithm::FnvModular);
    let quality = ObservedHash::new(HashAlgorithm::JLProjection { seed: 9 });
    let f = [shard(temp.path(), "fast.fsvi", &fast, &[(1, 3, "alpha")])];
    let q = [shard(
        temp.path(),
        "quality.fsvi",
        &quality,
        &[(2, 3, "alpha")],
    )];
    let reader = SemanticGenerationReader::open(Some(&f), Some(&q))?;
    let request = reader.activate_text(
        "alpha",
        TextQueryProducers::Progressive {
            fast: &fast,
            quality: &quality,
        },
    )?;
    let mut stream = request.progressive(2, None)?;
    let first = stream.next().unwrap()?;
    quality.fail.store(true, AtomicOrdering::SeqCst);
    assert!(stream.next().unwrap().is_err());
    assert!(stream.next().is_none());
    assert_eq!(ids(&first), [1]);
    quality.fail.store(false, AtomicOrdering::SeqCst);
    let mut stream = request.progressive(2, None)?;
    stream.next().unwrap()?;
    quality.drift.store(true, AtomicOrdering::SeqCst);
    assert!(stream.next().unwrap().is_err());
    assert!(stream.next().is_none());
    assert_eq!(
        quality.calls(),
        1,
        "second stream rejects drift without another quality call"
    );
    Ok(())
}

#[test]
fn text_filters_and_ann_fallback_use_retained_owners_after_rename() -> TestResult {
    let temp = tempfile::tempdir()?;
    let hash = ObservedHash::new(HashAlgorithm::FnvModular);
    let f = [shard(
        temp.path(),
        "fast.fsvi",
        &hash,
        &[(1, 4, "alpha"), (2, 3, "alpha beta")],
    )];
    let reader = SemanticGenerationReader::open(Some(&f), None)?;
    let saved = temp.path().join("retained.fsvi");
    fs::rename(&f[0].path, &saved)?;
    let before = fs::read(&saved)?;
    let request = reader.activate_text("alpha", TextQueryProducers::Fast(&hash))?;
    let filter = crate::search::vector_index::SemanticFilter {
        agents: None,
        workspaces: None,
        sources: Some(HashSet::from([3])),
        roles: None,
        created_from: None,
        created_to: None,
    };
    let exact = request.search(4, Some(&filter))?;
    let fallback = request.search_with_ann(4, Some(&filter), AnnSearchPolicy::default())?;
    assert_eq!(ids(&exact), [2]);
    assert_eq!(signature(&fallback), signature(&exact));
    assert!(
        fallback
            .execution()
            .iter()
            .all(|report| report.engine != SemanticShardEngine::NativeAnn)
    );
    assert!(!f[0].path.exists());
    assert_eq!(fs::read(&saved)?, before);
    assert!(!format!("{request:?}").contains("alpha"));
    Ok(())
}
