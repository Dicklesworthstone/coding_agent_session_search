mod current;

use super::*;
use crate::search::embedder::{EmbedderError, EmbedderResult};
use crate::search::semantic_reader::{SemanticResultPhase, SemanticScoreKind};
use crate::search::semantic_reader::ann::SemanticShardEngine;
use frankensearch::core::generation::EmbeddingIdentityBundleV1;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const MESSAGES: &[(u64, &str)] = &[
    (7, "authentication middleware"), (8, "second source passage"),
];

// Instrument real hash inference, retaining its exact producer identity and
// values. Only the deliberate failure switch replaces a call with an error.
struct CountedHash {
    inner: HashEmbedder,
    calls: AtomicUsize,
    fail: AtomicBool,
}

impl CountedHash {
    fn new(dimension: usize) -> Self {
        Self { inner: HashEmbedder::new(dimension), calls: AtomicUsize::new(0), fail: AtomicBool::new(false) }
    }
    fn calls(&self) -> usize { self.calls.load(Ordering::SeqCst) }
    fn reset(&self) { self.calls.store(0, Ordering::SeqCst); }
}

impl Embedder for CountedHash {
    fn embed_sync(&self, text: &str) -> EmbedderResult<Vec<f32>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            return Err(EmbedderError::EmbeddingFailed {
                model: self.inner.id().into(),
                source: Box::new(std::io::Error::other("injected inference failure")),
            });
        }
        self.inner.embed_sync(text)
    }
    fn embed_batch_sync(&self, texts: &[&str]) -> EmbedderResult<Vec<Vec<f32>>> {
        texts.iter().map(|text| self.embed_sync(text)).collect()
    }
    fn identity(&self) -> EmbedderResult<&EmbeddingIdentityBundleV1> { self.inner.identity() }
    fn dimension(&self) -> usize { self.inner.dimension() }
    fn id(&self) -> &str { self.inner.id() }
    fn is_semantic(&self) -> bool { self.inner.is_semantic() }
    fn category(&self) -> frankensearch::ModelCategory { self.inner.category() }
    fn tier(&self) -> frankensearch::ModelTier { self.inner.tier() }
}

fn two_tiers() -> TestResult<(tempfile::TempDir, Connection, SemanticGenerationReader, CountedHash, CountedHash)> {
    let (directory, conn, fast_batch) = fixture(MESSAGES)?;
    let fast = CountedHash::new(32);
    let quality = CountedHash::new(64);
    let mut identity = quality.identity()?.clone();
    identity.storage.format = "fsvi-v2".into();
    identity.storage.quantization = QuantizationFormat::F16;
    identity.storage.endianness = "little-endian".into();
    let binding = FsviV2IdentityBinding::new(fast_batch.reader.generation(), identity.freeze()?)?;
    let path = directory.path().join("quality.fsvi");
    let mut writer = VectorIndex::create_v2(&path, binding.clone())?;
    for hit in fast_batch.hits() {
        let text = MESSAGES.iter().find(|(id, _)| *id == hit.document.message_id).unwrap().1;
        writer.write_record(&hit.doc_id, &quality.inner.embed_sync(&canonicalize_for_embedding(text))?)?;
    }
    writer.finish()?;
    let witness = ValidatedFsviBytes::open_published(&path, &binding)?.witness().clone();
    let fast_tier = fast_batch.reader.fast.as_ref().unwrap();
    let fast_source = SemanticShardExpectation {
        path: directory.path().join("passages.fsvi"),
        binding: fast_tier.binding.clone(),
        witness: fast_tier.shards[0].witness().clone(),
    };
    let reader = SemanticGenerationReader::open(
        Some(&[fast_source]), Some(&[SemanticShardExpectation { path, binding, witness }]),
    )?;
    Ok((directory, conn, reader, fast, quality))
}

#[test]
fn verified_phases_are_lazy_and_keep_the_unhydrated_ranking() -> TestResult {
    let (directory, conn, reader, fast, quality) = two_tiers()?;
    let before = [fs::read(directory.path().join("passages.fsvi"))?, fs::read(directory.path().join("quality.fsvi"))?];
    let request = reader.activate_text("authentication middleware", TextQueryProducers::Progressive { fast: &fast, quality: &quality })?;
    let expected = request.search(2, None)?;
    fast.reset(); quality.reset();
    let mut transaction = conn.transaction()?;
    let mut phases = request.progressive_hydrated_in_transaction(2, None, &transaction,
        PassageProjection::RawPassagesV2, HydrationBudget::default())?;
    assert_eq!((fast.calls(), quality.calls()), (0, 0));
    let initial = phases.next().unwrap()?;
    assert_eq!(initial.source().coverage().phase, SemanticResultPhase::Initial);
    assert_eq!(initial.passages().len(), 2);
    assert_eq!((fast.calls(), quality.calls()), (1, 0));
    let refined = phases.next().unwrap()?;
    assert_eq!(refined.source().coverage().phase, SemanticResultPhase::Refined);
    assert_eq!(refined.source().hits(), expected.hits());
    assert_eq!(refined.source().score_kind(), SemanticScoreKind::ReciprocalRankFusion);
    assert_eq!((fast.calls(), quality.calls()), (1, 1));
    assert_eq!(phases.size_hint(), (0, Some(0)));
    assert!(phases.next().is_none());
    assert!(phases.next().is_none());
    drop(phases);
    fast.reset(); quality.reset();
    let mut stopped = request.progressive_hydrated_in_transaction(2, None, &transaction,
        PassageProjection::RawPassagesV2, HydrationBudget::default())?;
    stopped.next().unwrap()?;
    drop(stopped);
    assert_eq!((fast.calls(), quality.calls()), (1, 0));
    transaction.query("SELECT 1")?;
    transaction.rollback()?;
    assert_eq!(before, [fs::read(directory.path().join("passages.fsvi"))?, fs::read(directory.path().join("quality.fsvi"))?]);
    Ok(())
}

#[test]
fn initial_source_failure_never_starts_quality_and_never_resumes() -> TestResult {
    let (_directory, conn, reader, fast, quality) = two_tiers()?;
    conn.execute("UPDATE messages SET content = 'rewritten private body' WHERE id = 7")?;
    let request = reader.activate_text("authentication middleware", TextQueryProducers::Progressive { fast: &fast, quality: &quality })?;
    let mut transaction = conn.transaction()?;
    let mut phases = request.progressive_hydrated_in_transaction(2, None, &transaction,
        PassageProjection::RawPassagesV2, HydrationBudget::default())?;
    assert!(matches!(phases.next(), Some(Err(HydratedSearchError::Hydration(HydrationError::StaleSource)))));
    assert!(phases.next().is_none());
    assert!(phases.next().is_none());
    assert_eq!((fast.calls(), quality.calls()), (1, 0));
    drop(phases);
    transaction.rollback()?;
    Ok(())
}

#[test]
fn quality_inference_failure_cannot_relabel_initial_as_refined() -> TestResult {
    let (_directory, conn, reader, fast, quality) = two_tiers()?;
    quality.fail.store(true, Ordering::SeqCst);
    let request = reader.activate_text("authentication middleware", TextQueryProducers::Progressive { fast: &fast, quality: &quality })?;
    let mut transaction = conn.transaction()?;
    let mut phases = request.progressive_hydrated_in_transaction(2, None, &transaction,
        PassageProjection::RawPassagesV2, HydrationBudget::default())?;
    let initial = phases.next().unwrap()?;
    assert!(matches!(phases.next(), Some(Err(HydratedSearchError::Retrieval(_)))));
    assert!(phases.next().is_none());
    assert_eq!(initial.source().coverage().phase, SemanticResultPhase::Initial);
    assert_eq!((fast.calls(), quality.calls()), (1, 1));
    drop(phases);
    transaction.rollback()?;
    Ok(())
}

#[test]
fn source_changes_inside_caller_transaction_are_rejected_in_refinement() -> TestResult {
    let (_directory, conn, reader, fast, quality) = two_tiers()?;
    let request = reader.activate_text("authentication middleware", TextQueryProducers::Progressive { fast: &fast, quality: &quality })?;
    let mut transaction = conn.transaction()?;
    let mut phases = request.progressive_hydrated_in_transaction(2, None, &transaction,
        PassageProjection::RawPassagesV2, HydrationBudget::default())?;
    let initial = phases.next().unwrap()?;
    // Even a caller violating the stable-snapshot contract cannot smuggle new
    // bodies through refinement. Rollback below confines the write to this test.
    transaction.execute("UPDATE messages SET content = 'new text' WHERE id = 8")?;
    assert!(matches!(phases.next(), Some(Err(HydratedSearchError::Hydration(HydrationError::StaleSource)))));
    assert!(phases.next().is_none());
    assert_eq!(initial.source().coverage().phase, SemanticResultPhase::Initial);
    drop(phases);
    transaction.rollback()?;
    let mut transaction = conn.transaction()?;
    assert_eq!(request.search_hydrated_in_transaction(2, None, &transaction,
        PassageProjection::RawPassagesV2, HydrationBudget::default())?.passages().len(), 2);
    transaction.rollback()?;
    Ok(())
}

#[test]
fn initial_and_refined_do_not_receive_independent_byte_budgets() -> TestResult {
    let (_directory, conn, reader, fast, quality) = two_tiers()?;
    let source: usize = MESSAGES.iter().map(|(_, text)| text.len() + "assistantworkstationprivate/session.jsonl".len()).sum();
    let output: usize = MESSAGES.iter().map(|(_, text)| canonicalize_for_embedding(text).len() + "workstationprivate/session.jsonl".len()).sum();
    let request = reader.activate_text("authentication middleware", TextQueryProducers::Progressive { fast: &fast, quality: &quality })?;
    for budget in [
        HydrationBudget { max_total_source_bytes: source, ..Default::default() },
        HydrationBudget { max_total_output_bytes: output, ..Default::default() },
    ] {
        let mut transaction = conn.transaction()?;
        let mut phases = request.progressive_hydrated_in_transaction(2, None, &transaction,
            PassageProjection::RawPassagesV2, budget)?;
        let initial = phases.next().unwrap()?;
        assert_eq!(initial.passages().len(), 2);
        assert!(matches!(phases.next(), Some(Err(HydratedSearchError::Hydration(HydrationError::BudgetExceeded)))));
        assert!(phases.next().is_none());
        assert_eq!(initial.source().coverage().phase, SemanticResultPhase::Initial);
        drop(phases);
        transaction.rollback()?;
    }
    Ok(())
}

#[test]
fn impossible_progressive_budget_refuses_before_any_inference() -> TestResult {
    let (_directory, conn, reader, fast, quality) = two_tiers()?;
    let request = reader.activate_text("authentication middleware", TextQueryProducers::Progressive { fast: &fast, quality: &quality })?;
    let mut transaction = conn.transaction()?;
    assert!(matches!(request.progressive_hydrated_in_transaction(2, None, &transaction,
        PassageProjection::RawPassagesV2, HydrationBudget { max_passages: 3, ..Default::default() }),
        Err(HydratedSearchError::Hydration(HydrationError::BudgetExceeded))));
    assert_eq!((fast.calls(), quality.calls()), (0, 0));
    transaction.query("SELECT 1")?;
    transaction.rollback()?;
    Ok(())
}

#[test]
fn hydrated_ann_requests_preserve_exact_fallback_receipts() -> TestResult {
    let (_directory, conn, reader, fast, quality) = two_tiers()?;
    let request = reader.activate_text("authentication middleware", TextQueryProducers::Progressive { fast: &fast, quality: &quality })?;
    let policy = AnnSearchPolicy { initial_candidates: 8, max_candidates: 16 };
    let mut transaction = conn.transaction()?;
    let expected = request.search_hydrated_with_ann_in_transaction(2, None, policy, &transaction,
        PassageProjection::RawPassagesV2, HydrationBudget::default())?;
    let mut phases = request.progressive_hydrated_with_ann_in_transaction(2, None, policy, &transaction,
        PassageProjection::RawPassagesV2, HydrationBudget::default())?;
    let initial = phases.next().unwrap()?;
    let refined = phases.next().unwrap()?;
    for batch in [initial.source(), refined.source(), expected.source()] {
        assert!(!batch.execution().is_empty());
        assert!(batch.execution().iter().all(|report|
            report.engine == SemanticShardEngine::ExactFallback && report.ann_windows == 0));
    }
    assert_eq!(refined.source().hits(), expected.source().hits());
    assert!(phases.next().is_none());
    drop(phases);
    transaction.rollback()?;
    Ok(())
}
