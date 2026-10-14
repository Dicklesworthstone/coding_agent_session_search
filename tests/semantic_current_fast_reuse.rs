//! Execute the prior duplicate-scan path and current path against the same real
//! FSVI images, query, filter and native engine in one test invocation. Count
//! filter visits rather than claiming a wall-clock or whole-process RSS win.
#![cfg(any(target_os = "linux", target_os = "android"))]

use std::collections::BTreeSet;
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};

use coding_agent_search::franken_sync::compat::TransactionExt;
use coding_agent_search::franken_sync::{Connection, SqliteValue};
use coding_agent_search::search::canonicalize::{canonicalize_for_embedding, content_hash};
use coding_agent_search::search::embedder::Embedder;
use coding_agent_search::search::hash_embedder::HashEmbedder;
use coding_agent_search::search::semantic_reader::ann::{AnnSearchPolicy, SemanticShardEngine};
use coding_agent_search::search::semantic_reader::publication::text::hydration::{
    HydrationError, PassageProjection,
    query::current::{CurrentSearchError, CurrentTextQuery},
};
use coding_agent_search::search::semantic_reader::text::TextQueryProducers;
use coding_agent_search::search::semantic_reader::{
    SemanticGenerationReader, SemanticResultPhase, SemanticShardExpectation,
};
use coding_agent_search::search::vector_index::SemanticDocId;
use frankensearch::core::filter::SearchFilter;
use frankensearch::core::generation::{ArtifactGenerationIdentityV1, QuantizationFormat};
use frankensearch::index::{FsviV2IdentityBinding, ValidatedFsviBytes, VectorIndex};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;
const QUERY: &str = "authentication middleware";
const ROWS: &[(u64, &str)] = &[
    (7, QUERY),
    (8, "middleware routing handles authentication credentials"),
    (9, "canonical database transactions and durable storage"),
    (10, "terminal rendering and keyboard navigation"),
    (11, "query filters and ranking evidence"),
];

#[derive(Default)]
struct Visits {
    count: AtomicUsize,
    excluded: BTreeSet<u64>,
}

impl Visits {
    fn reset(&self) -> usize { self.count.swap(0, Ordering::SeqCst) }
    fn get(&self) -> usize { self.count.load(Ordering::SeqCst) }
}

impl SearchFilter for Visits {
    fn matches(&self, id: &str, _: Option<&serde_json::Value>) -> bool {
        self.count.fetch_add(1, Ordering::SeqCst);
        coding_agent_search::search::vector_index::parse_semantic_doc_id(id)
            .is_some_and(|doc| !self.excluded.contains(&doc.message_id))
    }
    fn name(&self) -> &str { "counted-current-source-scope" }
}

struct Fixture {
    dir: tempfile::TempDir,
    conn: Connection,
    reader: SemanticGenerationReader,
    fast: HashEmbedder,
    quality: HashEmbedder,
}

impl Fixture {
    fn new(quality_only: bool) -> TestResult<Self> {
        let dir = tempfile::tempdir()?;
        let conn = Connection::open(":memory:")?;
        conn.execute_batch("CREATE TABLE conversations (
            id INTEGER PRIMARY KEY, agent_id INTEGER, workspace_id INTEGER,
            source_id TEXT, origin_host TEXT, source_path TEXT);
            CREATE TABLE messages (id INTEGER PRIMARY KEY, conversation_id INTEGER,
            content TEXT, role TEXT, created_at INTEGER, idx INTEGER);
            INSERT INTO conversations VALUES (12, 3, 9, 'workstation', NULL, 'private/session.jsonl');")?;
        for &(id, text) in ROWS {
            conn.execute_with_params("INSERT INTO messages VALUES (?1, 12, ?2, 'assistant', 42, 0)",
                &[SqliteValue::Integer(id as i64), SqliteValue::Text(text.into())])?;
        }
        let fast = HashEmbedder::new(32);
        let quality = HashEmbedder::new(64);
        let make_shard = |name: &str, model: &HashEmbedder, rows: &[(u64, &str)]| -> TestResult<SemanticShardExpectation> {
            let mut identity = model.identity()?.clone();
            identity.storage.format = "fsvi-v2".into();
            identity.storage.quantization = QuantizationFormat::F16;
            identity.storage.endianness = "little-endian".into();
            let binding = FsviV2IdentityBinding::new(
                ArtifactGenerationIdentityV1::new(42, [7; 16])?, identity.freeze()?)?;
            let path = dir.path().join(name);
            let mut writer = VectorIndex::create_v2(&path, binding.clone())?;
            for &(id, text) in rows {
                let canonical = canonicalize_for_embedding(text);
                let doc = SemanticDocId {
                    message_id: id, chunk_idx: 0, agent_id: 3, workspace_id: 9,
                    source_id: crc32fast::hash(b"workstation"), role: coding_agent_search::search::vector_index::ROLE_ASSISTANT,
                    created_at_ms: 42, content_hash: Some(content_hash(&canonical)),
                };
                writer.write_record(&doc.to_doc_id_string(), &model.embed_bound_sync(&canonical)?.values)?;
            }
            writer.finish()?;
            let witness = ValidatedFsviBytes::open_published(&path, &binding)?.witness().clone();
            Ok(SemanticShardExpectation { path, binding, witness })
        };
        // Two shards per tier exercise the full retained cohort, not a one-file shortcut.
        let fast_shards = [make_shard("fast-a.fsvi", &fast, &ROWS[..2])?,
            make_shard("fast-b.fsvi", &fast, &ROWS[2..])?];
        let mut quality_tail = ROWS[2..].to_vec();
        if quality_only { quality_tail.push((6, QUERY)); }
        let quality_shards = [make_shard("quality-a.fsvi", &quality, &ROWS[..2])?,
            make_shard("quality-b.fsvi", &quality, &quality_tail)?];
        let reader = SemanticGenerationReader::open(Some(&fast_shards), Some(&quality_shards))?;
        Ok(Self { dir, conn, reader, fast, quality })
    }

    fn producers(&self) -> TextQueryProducers<'_> {
        TextQueryProducers::Progressive { fast: &self.fast, quality: &self.quality }
    }

    fn request<'a>(&'a self, filter: &'a Visits, k: usize) -> CurrentTextQuery<'a> {
        let mut request = CurrentTextQuery::new(QUERY, self.producers(), k, PassageProjection::RawPassagesV2);
        request.filter = Some(filter);
        request
    }
}

#[test]
fn unchanged_refinement_eliminates_one_complete_fast_scan_against_live_incumbent() -> TestResult {
    let fixture = Fixture::new(false)?;
    let visits = Visits::default();
    let request = fixture.reader.activate_text(QUERY, fixture.producers())?;
    // Verbatim incumbent control flow: Initial of one iterator, then Initial
    // plus Refined of a fresh iterator. Both use the SAME native implementation.
    let mut first = request.progressive(3, Some(&visits))?;
    let expected_initial = first.next().unwrap()?;
    drop(first);
    let fast_visits = visits.get();
    assert!(fast_visits > 0, "the control must really scan vectors");
    let mut second = request.progressive(3, Some(&visits))?;
    drop(second.next().unwrap()?);
    let expected_refined = second.next().unwrap()?;
    drop(second);
    let incumbent_visits = visits.reset();

    let mut tx = fixture.conn.transaction()?;
    let mut phases = fixture.reader.progressive_current_text_in_transaction(fixture.request(&visits, 3), &tx)?;
    let initial = phases.next().unwrap()?;
    assert_eq!(initial.verified().source().hits(), expected_initial.hits());
    assert_eq!(visits.get(), fast_visits);
    assert_eq!(initial.stats().fast_tier_reuses, 0);
    let refined = phases.next().unwrap()?;
    assert_eq!(refined.verified().source().hits(), expected_refined.hits());
    assert_eq!(refined.verified().source().coverage().phase, SemanticResultPhase::Refined);
    assert_eq!(refined.stats().searches, 2);
    assert_eq!(refined.stats().fast_tier_reuses, 1);
    assert_eq!(incumbent_visits, visits.get() + fast_visits);
    assert!(phases.next().is_none());
    drop(phases);
    tx.rollback()?;
    Ok(())
}

#[test]
fn source_change_after_initial_invalidates_fast_ranking_before_refusion() -> TestResult {
    let fixture = Fixture::new(false)?;
    let visits = Visits::default();
    let mut tx = fixture.conn.transaction()?;
    let mut phases = fixture.reader.progressive_current_text_in_transaction(fixture.request(&visits, 5), &tx)?;
    let initial = phases.next().unwrap()?;
    let edited = initial.verified().passages()[0].hit().document.message_id;
    // A caller violating the stable-snapshot contract cannot inject new text.
    // Source verification still runs when vector candidates are reused.
    tx.execute(&format!("UPDATE messages SET content = 'changed private text' WHERE id = {edited}"))?;
    let refined = phases.next().unwrap()?;
    let filtered = Visits { excluded: BTreeSet::from([edited]), ..Default::default() };
    let expected = fixture.reader.activate_text(QUERY, fixture.producers())?.search(5, Some(&filtered))?;
    assert_eq!(refined.verified().source().hits(), expected.hits());
    assert_eq!(refined.stats().searches, 3);
    assert_eq!(refined.stats().stale_passages, 1);
    assert_eq!(refined.stats().fast_tier_reuses, 1);
    assert!(refined.verified().passages().iter().all(|p| p.hit().document.message_id != edited));
    assert_eq!(initial.verified().source().coverage().phase, SemanticResultPhase::Initial);
    drop(phases);
    tx.rollback()?;
    Ok(())
}

#[test]
fn quality_only_missing_candidate_forces_fresh_filtered_fast_ranks() -> TestResult {
    let fixture = Fixture::new(true)?;
    let visits = Visits::default();
    let mut tx = fixture.conn.transaction()?;
    let mut phases = fixture.reader.progressive_current_text_in_transaction(fixture.request(&visits, 6), &tx)?;
    phases.next().unwrap()?;
    let refined = phases.next().unwrap()?;
    let filtered = Visits { excluded: BTreeSet::from([6]), ..Default::default() };
    let expected = fixture.reader.activate_text(QUERY, fixture.producers())?.search(6, Some(&filtered))?;
    assert_eq!(refined.verified().source().hits(), expected.hits());
    assert_eq!(refined.stats().missing_passages, 1);
    assert_eq!(refined.stats().searches, 3);
    assert_eq!(refined.stats().fast_tier_reuses, 1, "the expanded exclusion set must NOT reuse old ranks");
    drop(phases);
    tx.rollback()?;
    Ok(())
}

#[test]
fn refilled_initial_ranking_is_reused_without_widening_original_scope() -> TestResult {
    let fixture = Fixture::new(false)?;
    fixture.conn.execute("DELETE FROM messages WHERE id = 7")?;
    let visits = Visits { excluded: BTreeSet::from([11]), ..Default::default() };
    let mut tx = fixture.conn.transaction()?;
    let mut phases = fixture.reader.progressive_current_text_in_transaction(fixture.request(&visits, 5), &tx)?;
    let initial = phases.next().unwrap()?;
    assert_eq!(initial.stats().searches, 2);
    let refined = phases.next().unwrap()?;
    let filtered = Visits { excluded: BTreeSet::from([7, 11]), ..Default::default() };
    let expected = fixture.reader.activate_text(QUERY, fixture.producers())?.search(5, Some(&filtered))?;
    assert_eq!(refined.verified().source().hits(), expected.hits());
    assert_eq!(refined.stats().searches, 3);
    assert_eq!(refined.stats().fast_tier_reuses, 1);
    drop(phases);
    tx.rollback()?;
    Ok(())
}

#[test]
fn reuse_does_not_renew_hydration_byte_allowances() -> TestResult {
    let fixture = Fixture::new(false)?;
    let visits = Visits::default();
    let mut request = fixture.request(&visits, ROWS.len());
    request.hydration.max_total_source_bytes = ROWS.iter()
        .map(|(_, text)| text.len() + "assistantworkstationprivate/session.jsonl".len()).sum();
    let mut tx = fixture.conn.transaction()?;
    let mut phases = fixture.reader.progressive_current_text_in_transaction(request, &tx)?;
    phases.next().unwrap()?;
    assert!(matches!(phases.next(), Some(Err(CurrentSearchError::Hydration(HydrationError::BudgetExceeded)))));
    assert!(phases.next().is_none());
    drop(phases);
    tx.rollback()?;
    Ok(())
}

#[test]
fn reused_candidates_keep_retained_images_and_exact_fallback_receipts() -> TestResult {
    let fixture = Fixture::new(false)?;
    let paths = ["fast-a.fsvi", "fast-b.fsvi", "quality-a.fsvi", "quality-b.fsvi"];
    let before = paths.iter().map(|p| fs::read(fixture.dir.path().join(p))).collect::<Result<Vec<_>, _>>()?;
    let visits = Visits::default();
    let mut request = fixture.request(&visits, 3);
    request.ann = Some(AnnSearchPolicy { initial_candidates: 4, max_candidates: 8 });
    let mut tx = fixture.conn.transaction()?;
    let mut phases = fixture.reader.progressive_current_text_in_transaction(request, &tx)?;
    let initial = phases.next().unwrap()?;
    for path in paths {
        fs::rename(fixture.dir.path().join(path), fixture.dir.path().join(format!("retained-{path}")))?;
    }
    let refined = phases.next().unwrap()?;
    assert_eq!(refined.stats().fast_tier_reuses, 1);
    assert!(refined.verified().source().execution().iter().all(|e|
        e.engine == SemanticShardEngine::ExactFallback && e.ann_windows == 0));
    let initial_reports = serde_json::to_value(initial.verified().source().execution())?;
    let final_reports = serde_json::to_value(refined.verified().source().execution())?;
    assert_eq!(&final_reports.as_array().unwrap()[..2], initial_reports.as_array().unwrap());
    drop(phases);
    tx.rollback()?;
    for (path, bytes) in paths.iter().zip(before) {
        assert_eq!(fs::read(fixture.dir.path().join(format!("retained-{path}")))?, bytes);
    }
    Ok(())
}
