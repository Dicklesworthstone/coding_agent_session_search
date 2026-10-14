//! Current-source rank validation must cover candidates outside the visible page.
//! Each persisted vector is the real CASS hash embedding of its source passage.
//! Equal vectors make the documented shard-order tie rule deterministic. This
//! tests source/ranking consistency, not learned relevance or native ANN speed.
#![cfg(any(target_os = "linux", target_os = "android"))]

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

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
    SemanticGenerationReader, SemanticResultPhase, SemanticSearchBatch, SemanticShardExpectation,
};
use coding_agent_search::search::vector_index::{ROLE_ASSISTANT, SemanticDocId, parse_semantic_doc_id};
use frankensearch::core::filter::SearchFilter;
use frankensearch::core::generation::{ArtifactGenerationIdentityV1, QuantizationFormat};
use frankensearch::index::{FsviV2IdentityBinding, ValidatedFsviBytes, VectorIndex};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;
const TEXT: &str = "authentication middleware";
const K: usize = 4;
const HIDDEN: u64 = 9;

struct Exclude(BTreeSet<u64>);

impl SearchFilter for Exclude {
    fn matches(&self, id: &str, _: Option<&serde_json::Value>) -> bool {
        parse_semantic_doc_id(id).is_some_and(|doc| !self.0.contains(&doc.message_id))
    }

    fn name(&self) -> &str {
        "independent-known-source-exclusions"
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    paths: Vec<PathBuf>,
    conn: Connection,
    reader: SemanticGenerationReader,
    fast: HashEmbedder,
    quality: HashEmbedder,
}

fn ids(batch: &SemanticSearchBatch) -> Vec<u64> {
    batch.hits().iter().map(|hit| hit.document.message_id).collect()
}

impl Fixture {
    fn new(hidden_in_quality: bool) -> TestResult<Self> {
        let dir = tempfile::tempdir()?;
        let conn = Connection::open(":memory:")?;
        conn.execute_batch(
            "CREATE TABLE conversations (
                id INTEGER PRIMARY KEY, agent_id INTEGER, workspace_id INTEGER,
                source_id TEXT, origin_host TEXT, source_path TEXT);
             CREATE TABLE messages (
                id INTEGER PRIMARY KEY, conversation_id INTEGER, content TEXT,
                role TEXT, created_at INTEGER, idx INTEGER);
             INSERT INTO conversations VALUES
                (12, 3, 9, 'workstation', NULL, 'private/session.jsonl');",
        )?;
        for id in [1_i64, 2, 3, 4, 5, 9] {
            conn.execute_with_params(
                "INSERT INTO messages VALUES (?1, 12, ?2, 'assistant', 42, ?1)",
                &[SqliteValue::Integer(id), SqliteValue::Text(TEXT.into())],
            )?;
        }
        let fast = HashEmbedder::new(32);
        let quality = HashEmbedder::new(64);
        // Same values within a tier, different true producers across tiers.
        // No invented vector is stamped with the hash producer's identity.
        let canonical = canonicalize_for_embedding(TEXT);
        let build = |model: &HashEmbedder, prefix: &str, order: &[u64]| -> TestResult<Vec<SemanticShardExpectation>> {
            let mut identity = model.identity()?.clone();
            identity.storage.format = "fsvi-v2".into();
            identity.storage.quantization = QuantizationFormat::F16;
            identity.storage.endianness = "little-endian".into();
            let binding = FsviV2IdentityBinding::new(
                ArtifactGenerationIdentityV1::new(42, [7; 16])?, identity.freeze()?,
            )?;
            let vector = model.embed_bound_sync(&canonical)?.values;
            order.iter().enumerate().map(|(ordinal, &message_id)| {
                let path = dir.path().join(format!("{prefix}-{ordinal}.fsvi"));
                let document = SemanticDocId {
                    message_id, chunk_idx: 0, agent_id: 3, workspace_id: 9,
                    source_id: crc32fast::hash(b"workstation"), role: ROLE_ASSISTANT,
                    created_at_ms: 42, content_hash: Some(content_hash(&canonical)),
                };
                let mut writer = VectorIndex::create_v2(&path, binding.clone())?;
                writer.write_record(&document.to_doc_id_string(), &vector)?;
                writer.finish()?;
                let witness = ValidatedFsviBytes::open_published(&path, &binding)?.witness().clone();
                Ok(SemanticShardExpectation { path, binding: binding.clone(), witness })
            }).collect()
        };
        let with_hidden = [9, 2, 3, 1, 5];
        let without_hidden = [4, 1, 3, 2, 5];
        let (fast_order, quality_order) = if hidden_in_quality {
            (&without_hidden, &with_hidden)
        } else {
            (&with_hidden, &without_hidden)
        };
        let fast_shards = build(&fast, "fast", fast_order)?;
        let quality_shards = build(&quality, "quality", quality_order)?;
        let paths = fast_shards.iter().chain(&quality_shards).map(|shard| shard.path.clone()).collect();
        let reader = SemanticGenerationReader::open(Some(&fast_shards), Some(&quality_shards))?;
        Ok(Self { _dir: dir, paths, conn, reader, fast, quality })
    }

    fn producers(&self) -> TextQueryProducers<'_> {
        TextQueryProducers::Progressive { fast: &self.fast, quality: &self.quality }
    }

    fn request(&self) -> CurrentTextQuery<'_> {
        CurrentTextQuery::new(TEXT, self.producers(), K, PassageProjection::RawPassagesV2)
    }

    fn reference(&self, filter: Option<&dyn SearchFilter>) -> TestResult<SemanticSearchBatch> {
        Ok(self.reader.activate_text(TEXT, self.producers())?.search(K, filter)?)
    }

    fn control(&self) -> TestResult<SemanticSearchBatch> {
        let unfiltered = self.reference(None)?;
        // The final page is full and contains no stale/missing identity. A
        // final-page-only verifier therefore cannot notice the hidden source.
        assert_eq!(ids(&unfiltered), [1, 2, 3, 4]);
        assert!(!ids(&unfiltered).contains(&HIDDEN));
        let exclude = Exclude(BTreeSet::from([HIDDEN]));
        let expected = self.reference(Some(&exclude))?;
        assert_eq!(ids(&expected), [2, 1, 3, 4]);
        assert_ne!(unfiltered.hits(), expected.hits(), "the source exclusion must change actual ranks/scores");
        Ok(expected)
    }

    fn bytes(&self) -> TestResult<Vec<Vec<u8>>> {
        self.paths.iter().map(|path| Ok(fs::read(path)?)).collect()
    }
}

fn assert_page(actual: &SemanticSearchBatch, expected: &SemanticSearchBatch) {
    assert_eq!(actual.hits(), expected.hits(), "rank scores and both source ranks must match the independently filtered search");
    assert_eq!(actual.hits().len(), K);
    assert_eq!(actual.coverage().fast, expected.coverage().fast);
    assert_eq!(actual.coverage().quality, expected.coverage().quality);
    assert_eq!(actual.score_kind(), expected.score_kind());
}

#[test]
fn hidden_deleted_contributor_is_excluded_before_accepting_the_fused_page() -> TestResult {
    for hidden_in_quality in [false, true] {
        let fixture = Fixture::new(hidden_in_quality)?;
        let before = fixture.bytes()?;
        let expected = fixture.control()?;
        fixture.conn.execute("DELETE FROM messages WHERE id = 9")?;
        let mut tx = fixture.conn.transaction()?;
        let answer = fixture.reader.search_current_text_in_transaction(fixture.request(), &tx)?;
        assert_page(answer.verified().source(), &expected);
        assert_eq!(answer.verified().passages().len(), K);
        assert_eq!(answer.stats().missing_passages, 1);
        assert_eq!(answer.stats().searches, 2);
        tx.query("SELECT 1")?;
        tx.rollback()?;
        assert_eq!(fixture.bytes()?, before);
    }
    Ok(())
}

#[test]
fn hidden_rewritten_contributor_is_not_allowed_to_keep_old_tier_ranks() -> TestResult {
    let fixture = Fixture::new(false)?;
    let expected = fixture.control()?;
    fixture.conn.execute("UPDATE messages SET content = 'rewritten source' WHERE id = 9")?;
    let mut tx = fixture.conn.transaction()?;
    let answer = fixture.reader.search_current_text_in_transaction(fixture.request(), &tx)?;
    assert_page(answer.verified().source(), &expected);
    assert_eq!(answer.stats().stale_passages, 1);
    assert_eq!(answer.stats().searches, 2);
    tx.rollback()?;
    fixture.conn.execute_with_params("UPDATE messages SET content = ?1 WHERE id = 9", &[SqliteValue::Text(TEXT.into())])?;
    let mut tx = fixture.conn.transaction()?;
    let restored = fixture.reader.search_current_text_in_transaction(fixture.request(), &tx)?;
    assert_page(restored.verified().source(), &fixture.reference(None)?);
    assert_eq!(restored.stats().searches, 1, "exclusions must not leak into another request");
    tx.rollback()?;
    Ok(())
}

#[test]
fn hidden_quality_contributor_is_checked_during_progressive_refinement() -> TestResult {
    let fixture = Fixture::new(true)?;
    let expected = fixture.control()?;
    fixture.conn.execute("DELETE FROM messages WHERE id = 9")?;
    let mut tx = fixture.conn.transaction()?;
    let mut phases = fixture.reader.progressive_current_text_in_transaction(fixture.request(), &tx)?;
    let initial = phases.next().unwrap()?;
    assert_eq!(ids(initial.verified().source()), [4, 1, 3, 2]);
    assert_eq!(initial.stats().searches, 1);
    assert_eq!(initial.stats().missing_passages, 0);
    let refined = phases.next().unwrap()?;
    assert_page(refined.verified().source(), &expected);
    assert_eq!(refined.verified().source().coverage().phase, SemanticResultPhase::Refined);
    assert_eq!(refined.stats().searches, 3);
    assert_eq!(refined.stats().missing_passages, 1);
    assert_eq!(refined.stats().fast_tier_reuses, 1);
    assert_eq!(initial.verified().source().coverage().phase, SemanticResultPhase::Initial);
    assert!(phases.next().is_none());
    drop(phases);
    tx.rollback()?;
    Ok(())
}

#[test]
fn malformed_hidden_source_is_an_error_not_a_silent_exclusion() -> TestResult {
    let fixture = Fixture::new(true)?;
    fixture.control()?;
    fixture.conn.execute("UPDATE messages SET content = NULL WHERE id = 9")?;
    let mut tx = fixture.conn.transaction()?;
    assert!(matches!(fixture.reader.search_current_text_in_transaction(fixture.request(), &tx),
        Err(CurrentSearchError::Hydration(HydrationError::InvalidSource))));
    let mut phases = fixture.reader.progressive_current_text_in_transaction(fixture.request(), &tx)?;
    let initial = phases.next().unwrap()?;
    assert!(matches!(phases.next(), Some(Err(CurrentSearchError::Hydration(HydrationError::InvalidSource)))));
    assert!(phases.next().is_none());
    assert_eq!(initial.verified().source().coverage().phase, SemanticResultPhase::Initial);
    drop(phases);
    tx.query("SELECT 1")?;
    tx.rollback()?;
    Ok(())
}

#[test]
fn candidate_allowance_covers_hidden_rank_evidence_without_raising_the_limit() -> TestResult {
    let fixture = Fixture::new(false)?;
    fixture.control()?;
    let mut request = fixture.request();
    request.hydration.max_passages = K; // Visible page fits, five rank contributors do not.
    let mut tx = fixture.conn.transaction()?;
    assert!(matches!(fixture.reader.search_current_text_in_transaction(request, &tx),
        Err(CurrentSearchError::Hydration(HydrationError::BudgetExceeded))));
    tx.query("SELECT 1")?;
    tx.rollback()?;
    Ok(())
}

#[test]
fn identical_filtered_tier_windows_are_verified_once_and_keep_exact_coverage() -> TestResult {
    let fixture = Fixture::new(false)?;
    let filter = Exclude(BTreeSet::from([9, 4]));
    let expected = fixture.reference(Some(&filter))?;
    let mut request = fixture.request();
    request.filter = Some(&filter);
    request.hydration.max_passages = K; // Four shared IDs, not eight independent reads.
    let mut tx = fixture.conn.transaction()?;
    let answer = fixture.reader.search_current_text_in_transaction(request, &tx)?;
    assert_page(answer.verified().source(), &expected);
    assert_eq!(answer.stats().searches, 1);
    assert_eq!(answer.stats().source_bytes_read,
        K * (TEXT.len() + "assistantworkstationprivate/session.jsonl".len()));
    assert_eq!(answer.stats().output_bytes_checked,
        K * (canonicalize_for_embedding(TEXT).len() + "workstationprivate/session.jsonl".len()));
    tx.rollback()?;
    Ok(())
}

#[test]
fn requested_ann_fallback_preserves_rank_validation_and_execution_receipts() -> TestResult {
    let fixture = Fixture::new(false)?;
    let expected = fixture.control()?;
    fixture.conn.execute("DELETE FROM messages WHERE id = 9")?;
    let mut request = fixture.request();
    request.ann = Some(AnnSearchPolicy { initial_candidates: 4, max_candidates: 8 });
    let before = fixture.bytes()?;
    let mut tx = fixture.conn.transaction()?;
    let answer = fixture.reader.search_current_text_in_transaction(request, &tx)?;
    assert_page(answer.verified().source(), &expected);
    assert_eq!(answer.stats().searches, 2);
    assert_eq!(answer.verified().source().execution().len(), 10);
    assert!(answer.verified().source().execution().iter().all(|report|
        report.engine == SemanticShardEngine::ExactFallback && report.ann_windows == 0));
    tx.rollback()?;
    assert_eq!(fixture.bytes()?, before);
    Ok(())
}
