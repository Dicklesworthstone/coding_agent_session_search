use super::*;

fn source(content: &str) -> SourceMessage {
    SourceMessage {
        message_id: 7, conversation_id: 12, content: content.to_owned(), role: "assistant".into(),
        created_at: Some(42), agent_id: Some(3), workspace_id: Some(9),
        source_id: "workstation".into(), source_path: "private/session.jsonl".into(), idx: Some(0),
    }
}

fn identity(content: &str, chunk: u8, projection: PassageProjection) -> SemanticDocId {
    let canonical = match projection {
        PassageProjection::CanonicalPrefixV1 => canonicalize_for_embedding(content),
        PassageProjection::RawPassagesV2 => canonicalize_for_embedding(embedding_passages(content)[usize::from(chunk)]),
    };
    SemanticDocId {
        message_id: 7, chunk_idx: chunk, agent_id: 3, workspace_id: 9,
        source_id: crc32fast::hash(b"workstation"), role: role_code_from_str("assistant").unwrap(),
        created_at_ms: 42, content_hash: Some(content_hash(&canonical)),
    }
}

#[test]
fn same_primary_key_does_not_authorize_rewritten_text() {
    let expected = identity("original embedded passage", 0, PassageProjection::RawPassagesV2);
    assert!(verify_message(&expected, &source("original embedded passage"), PassageProjection::RawPassagesV2).is_ok());
    let error = verify_message(&expected, &source("private replacement secret"), PassageProjection::RawPassagesV2).unwrap_err();
    assert_eq!(error, HydrationError::StaleSource);
    assert!(!format!("{error:?} {error}").contains("private replacement secret"));
}

#[test]
fn all_encoded_metadata_dimensions_are_checked() {
    let expected = identity("matching passage", 0, PassageProjection::RawPassagesV2);
    for field in 0..6 {
        let mut actual = source("matching passage");
        match field {
            0 => actual.message_id += 1,
            1 => actual.agent_id = Some(4),
            2 => actual.workspace_id = Some(10),
            3 => actual.source_id = "other-source".into(),
            4 => actual.role = "user".into(),
            _ => actual.created_at = Some(43),
        }
        assert_eq!(verify_message(&expected, &actual, PassageProjection::RawPassagesV2), Err(HydrationError::StaleSource));
    }
}

#[test]
fn long_unicode_tail_is_verified_instead_of_whole_message_prefix() {
    let original = format!("{} unique last passage", "日本語 🦀 café ".repeat(400));
    let last = (embedding_passages(&original).len() - 1) as u8;
    let expected = identity(&original, last, PassageProjection::RawPassagesV2);
    let verified = verify_message(&expected, &source(&original), PassageProjection::RawPassagesV2).unwrap();
    assert!(verified.contains("unique last passage"));
    let changed = original.replace("unique last passage", "changed last passage");
    assert_eq!(verify_message(&expected, &source(&changed), PassageProjection::RawPassagesV2), Err(HydrationError::StaleSource));
}

#[test]
fn projection_is_explicit_and_not_silently_upgraded() {
    let original = "canonical prefix versus raw passages ".repeat(100);
    let expected = identity(&original, 0, PassageProjection::CanonicalPrefixV1);
    assert!(verify_message(&expected, &source(&original), PassageProjection::CanonicalPrefixV1).is_ok());
    assert_eq!(verify_message(&expected, &source(&original), PassageProjection::RawPassagesV2), Err(HydrationError::StaleSource));
    let mut wrong_chunk = expected;
    wrong_chunk.chunk_idx = 1;
    assert_eq!(verify_message(&wrong_chunk, &source(&original), PassageProjection::CanonicalPrefixV1), Err(HydrationError::StaleSource));
}

#[test]
fn canonical_equivalence_and_nullable_metadata_keep_indexer_semantics() {
    let mut expected = identity("matching passage whitespace", 0, PassageProjection::RawPassagesV2);
    expected.agent_id = 0;
    expected.workspace_id = u32::MAX;
    expected.created_at_ms = 0;
    let mut actual = source("matching  passage\nwhitespace");
    actual.agent_id = None;
    actual.workspace_id = Some(i64::MAX);
    actual.created_at = None;
    assert!(verify_message(&expected, &actual, PassageProjection::RawPassagesV2).is_ok());
    expected.content_hash = None;
    assert_eq!(verify_message(&expected, &actual, PassageProjection::RawPassagesV2), Err(HydrationError::MissingFingerprint));
}

#[test]
fn source_fallback_preserves_explicit_identity_and_local_case() {
    assert_eq!(canonical_source_id(Some(" LOCAL "), Some("remote-host")), "local");
    assert_eq!(canonical_source_id(Some(" remote-A "), Some("other")), "remote-A");
    assert_eq!(canonical_source_id(None, Some(" workstation ")), "workstation");
    assert_eq!(canonical_source_id(Some("  "), None), "local");
}

#[test]
fn budget_overflows_and_elapsed_requests_are_not_accepted() {
    assert_eq!(HydrationBudget { max_passages: usize::MAX, ..Default::default() }.validate(1), Err(HydrationError::InvalidBudget));
    assert_eq!(HydrationBudget { max_passages: 0, ..Default::default() }.validate(1), Err(HydrationError::BudgetExceeded));
    let mut work = Work { start: Instant::now(), budget: HydrationBudget::default(), source_bytes: usize::MAX, output_bytes: usize::MAX };
    assert_eq!(work.source(1), Err(HydrationError::BudgetExceeded));
    assert_eq!(work.output(1), Err(HydrationError::BudgetExceeded));
    work.budget.timeout = Duration::ZERO;
    assert_eq!(work.check(), Err(HydrationError::DeadlineExceeded));
}

// Real retained FSVI and real FrankenSQLite. The hash embedder is an explicit
// control producer: these are identity/ownership tests, not relevance evidence.
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
                &[SqliteValue::Integer(id as i64), SqliteValue::Text(content.into())],
            )?;
            for (chunk, passage) in embedding_passages(content).into_iter().enumerate() {
                let canonical = canonicalize_for_embedding(passage);
                let mut document = identity(content, chunk as u8, PassageProjection::RawPassagesV2);
                document.message_id = id;
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
    fn real_source_rewrite_fails_then_restoration_reuses_the_same_vector_owner() -> TestResult {
        let (directory, mut conn, batch) = fixture(&[(7, "authentication middleware")])?;
        let before = fs::read(directory.path().join("passages.fsvi"))?;
        let initial = batch.hydrate_from_connection(&mut conn, PassageProjection::RawPassagesV2, HydrationBudget::default())?;
        let score = initial.passages()[0].hit().ranking_score.to_bits();
        conn.execute("UPDATE messages SET content = 'private replacement text' WHERE id = 7")?;
        assert_eq!(batch.hydrate_from_connection(&mut conn, PassageProjection::RawPassagesV2, HydrationBudget::default()).unwrap_err(), HydrationError::StaleSource);
        // A failure must leave the caller's connection reusable.
        conn.execute("BEGIN")?;
        conn.execute("ROLLBACK")?;
        conn.execute("UPDATE messages SET content = 'authentication middleware' WHERE id = 7")?;
        let restored = batch.hydrate_from_connection(&mut conn, PassageProjection::RawPassagesV2, HydrationBudget::default())?;
        assert_eq!(restored.passages()[0].hit().ranking_score.to_bits(), score);
        assert_eq!(restored.passages()[0].canonical_text(), "authentication middleware");
        assert_eq!(before, fs::read(directory.path().join("passages.fsvi"))?);
        Ok(())
    }

    #[test]
    fn a_later_missing_message_cannot_return_an_earlier_verified_prefix() -> TestResult {
        let (_directory, mut conn, batch) = fixture(&[(7, "authentication middleware"), (8, "second source passage")])?;
        assert_eq!(batch.hydrate_from_connection(&mut conn, PassageProjection::RawPassagesV2, HydrationBudget::default())?.passages().len(), 2);
        conn.execute("DELETE FROM messages WHERE id = 8")?;
        assert_eq!(batch.hydrate_from_connection(&mut conn, PassageProjection::RawPassagesV2, HydrationBudget::default()).unwrap_err(), HydrationError::MissingSource);
        conn.execute("BEGIN")?;
        conn.execute("ROLLBACK")?;
        Ok(())
    }

    #[test]
    fn missing_conversation_and_changed_filter_metadata_are_rejected() -> TestResult {
        let (_directory, mut conn, batch) = fixture(&[(7, "authentication middleware")])?;
        conn.execute("UPDATE conversations SET source_id = 'foreign' WHERE id = 12")?;
        assert_eq!(batch.hydrate_from_connection(&mut conn, PassageProjection::RawPassagesV2, HydrationBudget::default()).unwrap_err(), HydrationError::StaleSource);
        conn.execute("DELETE FROM conversations WHERE id = 12")?;
        assert_eq!(batch.hydrate_from_connection(&mut conn, PassageProjection::RawPassagesV2, HydrationBudget::default()).unwrap_err(), HydrationError::MissingSource);
        Ok(())
    }

    #[test]
    fn source_and_output_byte_limits_are_enforced_before_returning_text() -> TestResult {
        let text = "日本語 authentication middleware";
        let (_directory, mut conn, batch) = fixture(&[(7, text)])?;
        for budget in [
            HydrationBudget { max_message_bytes: text.chars().count(), ..Default::default() },
            HydrationBudget { max_total_source_bytes: text.len(), ..Default::default() },
            HydrationBudget { max_total_output_bytes: 1, ..Default::default() },
        ] {
            assert_eq!(batch.hydrate_from_connection(&mut conn, PassageProjection::RawPassagesV2, budget).unwrap_err(), HydrationError::BudgetExceeded);
            conn.execute("BEGIN")?;
            conn.execute("ROLLBACK")?;
        }
        Ok(())
    }

    #[test]
    fn all_chunks_share_one_full_message_source_budget() -> TestResult {
        let text = "日本語 authentication middleware unrelated tail ".repeat(100);
        let (_directory, mut conn, batch) = fixture(&[(7, &text)])?;
        assert!(batch.hits().len() > 1);
        let metadata = "assistant".len() + "workstation".len() + "private/session.jsonl".len();
        let budget = HydrationBudget { max_total_source_bytes: text.len() + metadata, ..Default::default() };
        let hydrated = batch.hydrate_from_connection(&mut conn, PassageProjection::RawPassagesV2, budget)?;
        assert_eq!(hydrated.passages().len(), embedding_passages(&text).len());
        for passage in hydrated.passages() {
            assert!(Arc::ptr_eq(&hydrated.passages()[0].location, &passage.location));
        }
        Ok(())
    }

    #[test]
    fn explicit_snapshot_is_not_rolled_back_and_foreign_transaction_is_not_adopted() -> TestResult {
        let (_directory, mut conn, batch) = fixture(&[(7, "authentication middleware")])?;
        let mut transaction = conn.transaction()?;
        assert_eq!(batch.hydrate_in_transaction(&transaction, PassageProjection::RawPassagesV2, HydrationBudget::default())?.passages().len(), 1);
        transaction.rollback()?;
        drop(transaction);
        conn.execute("BEGIN")?;
        assert_eq!(batch.hydrate_from_connection(&mut conn, PassageProjection::RawPassagesV2, HydrationBudget::default()).unwrap_err(), HydrationError::Database);
        conn.execute("ROLLBACK")?;
        Ok(())
    }

    #[test]
    fn invalid_candidate_is_rejected_before_missing_source_is_consulted() -> TestResult {
        let (_directory, mut conn, mut batch) = fixture(&[(7, "authentication middleware")])?;
        conn.execute("DELETE FROM messages")?;
        batch.hits[0].ranking_score = f64::NAN;
        assert_eq!(batch.hydrate_from_connection(&mut conn, PassageProjection::RawPassagesV2, HydrationBudget::default()).unwrap_err(), HydrationError::InvalidCandidate);
        Ok(())
    }

    #[test]
    fn verified_output_retains_owners_after_paths_and_original_batch_are_gone() -> TestResult {
        let (directory, mut conn, batch) = fixture(&[(7, "authentication middleware")])?;
        let witness = batch.witness(TierKind::Fast, 0).unwrap().clone();
        fs::rename(directory.path().join("passages.fsvi"), directory.path().join("retained.fsvi"))?;
        let hydrated = batch.hydrate_from_connection(&mut conn, PassageProjection::RawPassagesV2, HydrationBudget::default())?;
        drop(batch);
        assert_eq!(hydrated.source().witness(TierKind::Fast, 0), Some(&witness));
        assert_eq!(hydrated.passages()[0].canonical_text(), "authentication middleware");
        assert!(!format!("{hydrated:?}").contains("authentication middleware"));
        assert!(!format!("{:?}", hydrated.passages()[0]).contains("authentication middleware"));
        assert!(!format!("{:?}", hydrated.passages()[0].location()).contains("private/session.jsonl"));
        Ok(())
    }

    #[test]
    fn zero_budget_deadline_does_not_open_a_transaction() -> TestResult {
        let (_directory, mut conn, batch) = fixture(&[(7, "authentication middleware")])?;
        let budget = HydrationBudget { timeout: Duration::ZERO, ..Default::default() };
        assert_eq!(batch.hydrate_from_connection(&mut conn, PassageProjection::RawPassagesV2, budget).unwrap_err(), HydrationError::DeadlineExceeded);
        conn.execute("BEGIN")?;
        conn.execute("ROLLBACK")?;
        Ok(())
    }
}
