use super::*;

#[test]
fn admission_refuses_overflow_and_impossible_pages_before_native_work() {
    let budget = HydrationBudget { max_passages: 4, ..Default::default() };
    assert!(prepare(2, 2, budget).is_ok());
    assert!(matches!(prepare(3, 2, budget), Err(HydrationError::BudgetExceeded)));
    assert!(matches!(prepare(usize::MAX, 2, budget), Err(HydrationError::BudgetExceeded)));
    assert!(matches!(prepare(1, 1, HydrationBudget {
        timeout: Duration::ZERO, ..Default::default()
    }), Err(HydrationError::DeadlineExceeded)));
}

#[test]
fn passage_and_byte_accounts_are_cumulative_not_per_phase() {
    let mut work = prepare(2, 2, HydrationBudget {
        max_passages: 4, max_total_source_bytes: 5,
        max_total_output_bytes: 3, ..Default::default()
    }).unwrap();
    work.passages(2).unwrap();
    work.passages(2).unwrap();
    assert_eq!(work.passages(1), Err(HydrationError::BudgetExceeded));
    work.source(3).unwrap();
    work.source(2).unwrap();
    assert_eq!(work.source(1), Err(HydrationError::BudgetExceeded));
    work.output(2).unwrap();
    work.output(1).unwrap();
    assert_eq!(work.output(1), Err(HydrationError::BudgetExceeded));
}

// Real source rows and retained vector images, with the explicitly non-semantic
// CASS hash producer. No model download or learned relevance claim is involved.
#[cfg(any(target_os = "linux", target_os = "android"))]
mod retained {
    use super::*;
    use crate::franken_sync::SqliteValue;
    use crate::search::embedder::Embedder;
    use crate::search::hash_embedder::HashEmbedder;
    use crate::search::semantic_reader::text::TextQueryProducers;
    use crate::search::semantic_reader::{SemanticGenerationReader, SemanticShardExpectation};
    use frankensearch::core::generation::{ArtifactGenerationIdentityV1, QuantizationFormat};
    use frankensearch::index::{FsviV2IdentityBinding, ValidatedFsviBytes, VectorIndex};
    use std::fs;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn fixture(messages: &[(u64, &str)]) -> TestResult<(tempfile::TempDir, Connection, SemanticSearchBatch)> {
        let directory = tempfile::tempdir()?;
        let conn = Connection::open(":memory:")?;
        conn.execute_batch("CREATE TABLE conversations (
            id INTEGER PRIMARY KEY, agent_id INTEGER, workspace_id INTEGER,
            source_id TEXT, origin_host TEXT, source_path TEXT);
            CREATE TABLE messages (id INTEGER PRIMARY KEY, conversation_id INTEGER,
            content TEXT, role TEXT, created_at INTEGER, idx INTEGER);
            INSERT INTO conversations VALUES (12, 3, 9, 'workstation', NULL, 'private/session.jsonl');")?;
        let model = HashEmbedder::new(32);
        let mut producer = model.identity()?.clone();
        producer.storage.format = "fsvi-v2".into();
        producer.storage.quantization = QuantizationFormat::F16;
        producer.storage.endianness = "little-endian".into();
        let binding = FsviV2IdentityBinding::new(
            ArtifactGenerationIdentityV1::new(7, [3; 16])?, producer.freeze()?,
        )?;
        let path = directory.path().join("passages.fsvi");
        let mut writer = VectorIndex::create_v2(&path, binding.clone())?;
        let mut count = 0;
        for &(id, content) in messages {
            conn.execute_with_params(
                "INSERT INTO messages VALUES (?1, 12, ?2, 'assistant', 42, 0)",
                &[SqliteValue::Integer(i64::try_from(id)?), SqliteValue::Text(content.into())],
            )?;
            for (chunk, passage) in embedding_passages(content).into_iter().enumerate() {
                let canonical = canonicalize_for_embedding(passage);
                let document = SemanticDocId {
                    message_id: id, chunk_idx: u8::try_from(chunk)?, agent_id: 3,
                    workspace_id: 9, source_id: crc32fast::hash(b"workstation"),
                    role: role_code_from_str("assistant").unwrap(), created_at_ms: 42,
                    content_hash: Some(content_hash(&canonical)),
                };
                writer.write_record(&document.to_doc_id_string(), &model.embed_bound_sync(&canonical)?.values)?;
                count += 1;
            }
        }
        writer.finish()?;
        let witness = ValidatedFsviBytes::open_published(&path, &binding)?.witness().clone();
        let reader = SemanticGenerationReader::open(Some(&[SemanticShardExpectation { path, binding, witness }]), None)?;
        let batch = reader.activate_text("authentication middleware", TextQueryProducers::Fast(&model))?.search(count, None)?;
        assert_eq!(batch.hits().len(), count);
        Ok((directory, conn, batch))
    }

    #[test]
    fn composed_text_query_keeps_scores_and_rejects_rewritten_sources() -> TestResult {
        let (directory, conn, batch) = fixture(&[(7, "authentication middleware")])?;
        let before = fs::read(directory.path().join("passages.fsvi"))?;
        let model = HashEmbedder::new(32);
        let request = batch.reader.activate_text(
            "authentication middleware", TextQueryProducers::Fast(&model),
        )?;
        let expected = request.search(1, None)?;
        let mut transaction = conn.transaction()?;
        let hydrated = request.search_hydrated_in_transaction(
            1, None, &transaction, PassageProjection::RawPassagesV2, HydrationBudget::default(),
        )?;
        assert_eq!(hydrated.passages().len(), 1);
        assert_eq!(hydrated.passages()[0].canonical_text(), "authentication middleware");
        assert_eq!(hydrated.source().hits(), expected.hits());
        transaction.rollback()?;

        conn.execute("UPDATE messages SET content = 'private replacement' WHERE id = 7")?;
        let mut transaction = conn.transaction()?;
        assert!(matches!(request.search_hydrated_in_transaction(
            1, None, &transaction, PassageProjection::RawPassagesV2, HydrationBudget::default(),
        ), Err(HydratedSearchError::Hydration(HydrationError::StaleSource))));
        // Error handling leaves transaction ownership with the caller.
        transaction.query("SELECT 1")?;
        transaction.rollback()?;
        conn.execute("UPDATE messages SET content = 'authentication middleware' WHERE id = 7")?;
        let mut transaction = conn.transaction()?;
        assert_eq!(request.search_hydrated_in_transaction(
            1, None, &transaction, PassageProjection::RawPassagesV2, HydrationBudget::default(),
        )?.source().hits(), expected.hits());
        transaction.rollback()?;
        assert_eq!(before, fs::read(directory.path().join("passages.fsvi"))?);
        Ok(())
    }

    #[test]
    fn composed_query_never_returns_a_prefix_when_a_later_message_is_missing() -> TestResult {
        let (_directory, conn, batch) = fixture(&[
            (7, "authentication middleware"), (8, "second source passage"),
        ])?;
        let model = HashEmbedder::new(32);
        let request = batch.reader.activate_text(
            "authentication middleware", TextQueryProducers::Fast(&model),
        )?;
        conn.execute("DELETE FROM messages WHERE id = 8")?;
        let mut transaction = conn.transaction()?;
        assert!(matches!(request.search_hydrated_in_transaction(
            2, None, &transaction, PassageProjection::RawPassagesV2, HydrationBudget::default(),
        ), Err(HydratedSearchError::Hydration(HydrationError::MissingSource))));
        transaction.query("SELECT 1")?;
        transaction.rollback()?;
        Ok(())
    }
}
