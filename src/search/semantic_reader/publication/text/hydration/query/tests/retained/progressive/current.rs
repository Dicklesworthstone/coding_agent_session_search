//! Stale-hit refill through the actual lazy reader, FSVI files and canonical SQL.
//! Hash inference is real but explicitly non-semantic. No native-model latency
//! or graph-acceleration claim is attached to these fixtures.

use super::*;
use crate::search::semantic_reader::publication::text::hydration::query::current::{
    CurrentSearchError, CurrentTextQuery,
};
use std::collections::BTreeSet;

fn query<'a>(fast: &'a CountedHash, quality: &'a CountedHash, k: usize) -> CurrentTextQuery<'a> {
    CurrentTextQuery::new(
        "authentication middleware",
        TextQueryProducers::Progressive { fast, quality },
        k,
        PassageProjection::RawPassagesV2,
    )
}

struct Allowed(BTreeSet<String>);

impl SearchFilter for Allowed {
    fn matches(&self, id: &str, _: Option<&serde_json::Value>) -> bool {
        self.0.contains(id)
    }

    fn name(&self) -> &str {
        "allowed"
    }
}

#[test]
fn current_progressive_refills_initial_then_matches_independent_filtered_fusion() -> TestResult {
    let (_directory, conn, reader, fast, quality) = two_tiers()?;
    let original = reader.activate_text("authentication middleware", TextQueryProducers::Progressive {
        fast: &fast, quality: &quality,
    })?.search(2, None)?;
    let scope = Allowed(original.hits().iter()
        .filter(|hit| hit.document.message_id != 7)
        .map(|hit| hit.doc_id.clone()).collect());
    let expected = reader.activate_text("authentication middleware", TextQueryProducers::Progressive {
        fast: &fast, quality: &quality,
    })?.search(1, Some(&scope))?;
    conn.execute("UPDATE messages SET content = 'changed' WHERE id = 7")?;
    fast.reset();
    quality.reset();
    let mut transaction = conn.transaction()?;
    let mut phases = reader.progressive_current_text_in_transaction(query(&fast, &quality, 1), &transaction)?;
    assert_eq!((fast.calls(), quality.calls()), (0, 0));
    let initial = phases.next().unwrap()?;
    assert_eq!(initial.verified().source().coverage().phase, SemanticResultPhase::Initial);
    assert_eq!(initial.verified().passages()[0].hit().document.message_id, 8);
    assert_eq!((initial.stats().searches, initial.stats().stale_passages), (2, 1));
    assert_eq!((fast.calls(), quality.calls()), (1, 0));
    let refined = phases.next().unwrap()?;
    assert_eq!(refined.verified().source().coverage().phase, SemanticResultPhase::Refined);
    assert_eq!(refined.verified().source().hits(), expected.hits());
    assert_eq!(refined.verified().source().score_kind(), SemanticScoreKind::ReciprocalRankFusion);
    assert_eq!((refined.stats().searches, refined.stats().stale_passages), (3, 1));
    assert!(refined.stats().source_bytes_read > initial.stats().source_bytes_read);
    assert_eq!((fast.calls(), quality.calls()), (1, 1));
    assert_eq!(initial.verified().source().coverage().phase, SemanticResultPhase::Initial);
    assert_eq!(phases.size_hint(), (0, Some(0)));
    assert!(phases.next().is_none());
    assert!(phases.next().is_none());
    drop(phases);
    transaction.query("SELECT 1")?;
    transaction.rollback()?;
    Ok(())
}

#[test]
fn dropping_a_refilled_initial_page_never_starts_quality() -> TestResult {
    let (_directory, conn, reader, fast, quality) = two_tiers()?;
    conn.execute("DELETE FROM messages WHERE id = 7")?;
    let mut transaction = conn.transaction()?;
    let mut phases = reader.progressive_current_text_in_transaction(query(&fast, &quality, 1), &transaction)?;
    let initial = phases.next().unwrap()?;
    assert_eq!((initial.stats().searches, initial.stats().missing_passages), (2, 1));
    assert_eq!(initial.verified().passages()[0].hit().document.message_id, 8);
    drop(phases);
    assert_eq!((fast.calls(), quality.calls()), (1, 0));
    transaction.rollback()?;
    Ok(())
}

// A partially populated fast tier cannot vouch for a quality-only document.
// Both tiers are independently written using their actual hash producer.
fn partial_tiers() -> TestResult<(tempfile::TempDir, Connection, SemanticGenerationReader, CountedHash, CountedHash)> {
    let fast_rows = [(8, "authentication middleware routing"), (9, "second source passage")];
    let (directory, conn, fast_batch) = fixture(&fast_rows)?;
    let fast = CountedHash::new(32);
    let quality = CountedHash::new(64);
    conn.execute("INSERT INTO messages VALUES (7, 12, 'authentication middleware', 'assistant', 42, 0)")?;
    let mut identity = quality.identity()?.clone();
    identity.storage.format = "fsvi-v2".into();
    identity.storage.quantization = QuantizationFormat::F16;
    identity.storage.endianness = "little-endian".into();
    let binding = FsviV2IdentityBinding::new(fast_batch.reader.generation(), identity.freeze()?)?;
    let path = directory.path().join("quality.fsvi");
    let mut writer = VectorIndex::create_v2(&path, binding.clone())?;
    for (id, text) in [(7, "authentication middleware"), fast_rows[0], fast_rows[1]] {
        let canonical = canonicalize_for_embedding(text);
        let document = SemanticDocId {
            message_id: id, chunk_idx: 0, agent_id: 3, workspace_id: 9,
            source_id: crc32fast::hash(b"workstation"),
            role: role_code_from_str("assistant").unwrap(), created_at_ms: 42,
            content_hash: Some(content_hash(&canonical)),
        };
        writer.write_record(&document.to_doc_id_string(), &quality.inner.embed_sync(&canonical)?)?;
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
fn quality_only_missing_hit_triggers_refusion_without_repeating_either_inference() -> TestResult {
    let (_directory, conn, reader, fast, quality) = partial_tiers()?;
    let request = reader.activate_text("authentication middleware", TextQueryProducers::Progressive {
        fast: &fast, quality: &quality,
    })?;
    let before = request.search(2, None)?;
    assert!(before.hits().iter().any(|hit| hit.document.message_id == 7),
        "sensitivity control: missing quality-only candidate must reach the unfiltered fused page");
    let all = request.search(3, None)?;
    let scope = Allowed(all.hits().iter()
        .filter(|hit| hit.document.message_id != 7)
        .map(|hit| hit.doc_id.clone()).collect());
    let expected = request.search(2, Some(&scope))?;
    conn.execute("DELETE FROM messages WHERE id = 7")?;
    fast.reset();
    quality.reset();
    let mut transaction = conn.transaction()?;
    let mut phases = reader.progressive_current_text_in_transaction(query(&fast, &quality, 2), &transaction)?;
    let initial = phases.next().unwrap()?;
    assert_eq!(initial.stats().searches, 1);
    assert_eq!(initial.stats().missing_passages, 0);
    assert_eq!((fast.calls(), quality.calls()), (1, 0));
    let refined = phases.next().unwrap()?;
    assert_eq!(refined.verified().source().hits(), expected.hits());
    assert_eq!((refined.stats().searches, refined.stats().missing_passages), (3, 1));
    assert_eq!((fast.calls(), quality.calls()), (1, 1));
    assert!(phases.next().is_none());
    drop(phases);
    transaction.rollback()?;
    Ok(())
}

#[test]
fn original_scope_survives_both_phases_and_quality_side_refill() -> TestResult {
    let (_directory, conn, reader, fast, quality) = partial_tiers()?;
    let all = reader.activate_text("authentication middleware", TextQueryProducers::Progressive {
        fast: &fast, quality: &quality,
    })?.search(3, None)?;
    let scope = Allowed(all.hits().iter()
        .filter(|hit| hit.document.message_id != 9)
        .map(|hit| hit.doc_id.clone()).collect());
    conn.execute("DELETE FROM messages WHERE id = 7")?;
    fast.reset();
    quality.reset();
    let mut request = query(&fast, &quality, 2);
    request.filter = Some(&scope);
    let mut transaction = conn.transaction()?;
    let mut phases = reader.progressive_current_text_in_transaction(request, &transaction)?;
    for _ in 0..2 {
        let result = phases.next().unwrap()?;
        assert_eq!(result.verified().passages().len(), 1);
        assert_eq!(result.verified().passages()[0].hit().document.message_id, 8);
    }
    assert_eq!((fast.calls(), quality.calls()), (1, 1));
    assert!(phases.next().is_none());
    drop(phases);
    transaction.rollback()?;
    Ok(())
}

#[test]
fn initial_refill_cannot_renew_search_or_passage_fuel_for_refinement() -> TestResult {
    let (_directory, conn, reader, fast, quality) = two_tiers()?;
    conn.execute("DELETE FROM messages WHERE id = 7")?;
    let mut transaction = conn.transaction()?;
    for constrain_searches in [true, false] {
        fast.reset();
        quality.reset();
        let mut request = query(&fast, &quality, 1);
        if constrain_searches {
            request.refill.max_searches = 2;
        } else {
            request.hydration.max_passages = 2;
        }
        let mut phases = reader.progressive_current_text_in_transaction(request, &transaction)?;
        let initial = phases.next().unwrap()?;
        assert_eq!(initial.stats().searches, 2);
        let failure = phases.next().unwrap();
        if constrain_searches {
            assert!(matches!(failure, Err(CurrentSearchError::RefillExhausted)));
        } else {
            assert!(matches!(failure, Err(CurrentSearchError::Hydration(HydrationError::BudgetExceeded))));
        }
        assert_eq!((fast.calls(), quality.calls()), (1, 0), "refused phase must not start quality");
        assert_eq!(initial.verified().source().coverage().phase, SemanticResultPhase::Initial);
        assert!(phases.next().is_none());
    }
    transaction.rollback()?;
    Ok(())
}

#[test]
fn source_and_output_bytes_are_cumulative_across_refill_and_phase_boundaries() -> TestResult {
    let (_directory, conn, reader, fast, quality) = two_tiers()?;
    conn.execute("UPDATE messages SET content = 'changed' WHERE id = 7")?;
    let mut transaction = conn.transaction()?;
    let mut probe = reader.progressive_current_text_in_transaction(query(&fast, &quality, 1), &transaction)?;
    let first = probe.next().unwrap()?;
    let stats = *first.stats();
    assert!(stats.source_bytes_read > 0 && stats.output_bytes_checked > 0);
    drop(probe);
    for constrain_source in [true, false] {
        let mut request = query(&fast, &quality, 1);
        if constrain_source {
            request.hydration.max_total_source_bytes = stats.source_bytes_read;
        } else {
            request.hydration.max_total_output_bytes = stats.output_bytes_checked;
        }
        let mut phases = reader.progressive_current_text_in_transaction(request, &transaction)?;
        let initial = phases.next().unwrap()?;
        assert_eq!(initial.stats(), &stats);
        assert!(matches!(
            phases.next(),
            Some(Err(CurrentSearchError::Hydration(HydrationError::BudgetExceeded)))
        ));
        assert!(phases.next().is_none());
    }
    transaction.rollback()?;
    Ok(())
}

#[test]
fn quality_failure_is_terminal_and_does_not_upgrade_the_initial_receipt() -> TestResult {
    let (_directory, conn, reader, fast, quality) = two_tiers()?;
    quality.fail.store(true, Ordering::SeqCst);
    let mut transaction = conn.transaction()?;
    let mut phases = reader.progressive_current_text_in_transaction(query(&fast, &quality, 1), &transaction)?;
    let initial = phases.next().unwrap()?;
    assert!(matches!(phases.next(), Some(Err(CurrentSearchError::Retrieval(_)))));
    quality.fail.store(false, Ordering::SeqCst);
    assert!(phases.next().is_none());
    assert_eq!((fast.calls(), quality.calls()), (1, 1));
    assert_eq!(initial.verified().source().coverage().phase, SemanticResultPhase::Initial);
    drop(phases);
    transaction.rollback()?;
    Ok(())
}

#[test]
fn impossible_two_phase_admission_refuses_before_inference_and_zero_k_stays_lazy() -> TestResult {
    let (_directory, conn, reader, fast, quality) = two_tiers()?;
    let mut transaction = conn.transaction()?;
    let mut one_search = query(&fast, &quality, 1);
    one_search.refill.max_searches = 1;
    assert!(matches!(
        reader.progressive_current_text_in_transaction(one_search, &transaction),
        Err(CurrentSearchError::InvalidRefillBudget)
    ));
    let mut one_passage = query(&fast, &quality, 1);
    one_passage.hydration.max_passages = 1;
    assert!(matches!(
        reader.progressive_current_text_in_transaction(one_passage, &transaction),
        Err(CurrentSearchError::Hydration(HydrationError::BudgetExceeded))
    ));
    assert!(matches!(
        reader.progressive_current_text_in_transaction(query(&fast, &quality, usize::MAX), &transaction),
        Err(CurrentSearchError::Hydration(HydrationError::BudgetExceeded))
    ));
    let fast_only = CurrentTextQuery::new(
        "authentication middleware", TextQueryProducers::Fast(&fast), 1,
        PassageProjection::RawPassagesV2,
    );
    assert!(matches!(
        reader.progressive_current_text_in_transaction(fast_only, &transaction),
        Err(CurrentSearchError::Retrieval(SemanticReaderError::ProgressiveRequiresBothTiers))
    ));
    assert_eq!((fast.calls(), quality.calls()), (0, 0));
    let mut phases = reader.progressive_current_text_in_transaction(query(&fast, &quality, 0), &transaction)?;
    let initial = phases.next().unwrap()?;
    let refined = phases.next().unwrap()?;
    assert!(initial.verified().passages().is_empty());
    assert!(refined.verified().passages().is_empty());
    assert_eq!((initial.stats().searches, refined.stats().searches), (1, 2));
    assert_eq!((fast.calls(), quality.calls()), (0, 0));
    assert!(phases.next().is_none());
    drop(phases);
    transaction.rollback()?;
    Ok(())
}

#[test]
fn retained_images_and_ann_fallback_receipts_survive_refill_after_path_renames() -> TestResult {
    let (directory, conn, reader, fast, quality) = two_tiers()?;
    let fast_path = directory.path().join("passages.fsvi");
    let quality_path = directory.path().join("quality.fsvi");
    let before_fast = fs::read(&fast_path)?;
    let before_quality = fs::read(&quality_path)?;
    conn.execute("DELETE FROM messages WHERE id = 7")?;
    let mut request = query(&fast, &quality, 1);
    request.ann = Some(AnnSearchPolicy { initial_candidates: 1, max_candidates: 2 });
    let mut transaction = conn.transaction()?;
    let mut phases = reader.progressive_current_text_in_transaction(request, &transaction)?;
    let retained_fast = directory.path().join("retained-fast.fsvi");
    let retained_quality = directory.path().join("retained-quality.fsvi");
    fs::rename(&fast_path, &retained_fast)?;
    let initial = phases.next().unwrap()?;
    fs::rename(&quality_path, &retained_quality)?;
    let refined = phases.next().unwrap()?;
    for result in [&initial, &refined] {
        assert_eq!(result.verified().passages()[0].hit().document.message_id, 8);
        assert!(result.verified().source().execution().iter().all(|report|
            report.engine == SemanticShardEngine::ExactFallback && report.ann_windows == 0));
    }
    assert_eq!((fast.calls(), quality.calls()), (1, 1));
    assert!(phases.next().is_none());
    drop(phases);
    transaction.rollback()?;
    assert_eq!(fs::read(retained_fast)?, before_fast);
    assert_eq!(fs::read(retained_quality)?, before_quality);
    Ok(())
}
