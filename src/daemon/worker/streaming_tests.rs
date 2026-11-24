//! Exercise the worker's real hash embedder, private FSVI writer, and durable
//! progress writes across first builds and streamed canonical reconciliation.

use super::*;
use crate::storage::sqlite::MessageForEmbedding;

#[test]
fn first_build_streaming_keeps_cancelled_batches_private_and_retries_exactly() -> anyhow::Result<()>
{
    let messages: Vec<_> = (1..=EMBED_PROGRESS_CHUNK_SIZE * 2 + 1)
        .map(|ordinal| MessageForEmbedding {
            message_id: i64::try_from(ordinal).unwrap(),
            created_at: Some(1_700_000_000_000),
            agent_id: 3,
            workspace_id: Some(5),
            source_id_hash: 7,
            role: "assistant".into(),
            content: format!("compiler diagnostic document {ordinal} with independent evidence"),
        })
        .collect();
    let reference = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let mut reference_inputs = Vec::new();
    for message in &messages {
        reference_inputs.extend(
            canonical_message_inputs(message)?
                .into_iter()
                .map(|(_, input)| input),
        );
    }
    let expected = indexer
        .build_and_save_index(indexer.embed_messages(&reference_inputs)?, reference.path())?;
    assert_eq!(expected.record_count(), messages.len());

    for cancel_after in [EMBED_PROGRESS_CHUNK_SIZE, messages.len()] {
        let temp = tempfile::tempdir()?;
        let db_path = temp.path().join("archive.db");
        let index_path = temp.path().join("semantic");
        let live = vector_index_path(&index_path, indexer.embedder_id());
        let storage = FrankenStorage::open(&db_path)?;
        let (worker, _) = EmbeddingWorker::new();
        let total = i64::try_from(messages.len())?;
        let job_id = storage.upsert_embedding_job(&db_path.to_string_lossy(), "hash", total)?;
        storage.start_embedding_job(job_id)?;
        let mut cancelled_progress = Vec::new();
        let result = worker.generate_embeddings_from_source(
            &storage,
            &embedding_source::SliceEmbeddingSource(&messages),
            "hash",
            false,
            job_id,
            &index_path,
            &db_path,
            TierKind::Fast,
            &mut |completed| {
                assert!(!live.exists(), "an incomplete first build became visible");
                cancelled_progress.push(completed);
                if completed >= i64::try_from(cancel_after).unwrap() {
                    worker.cancel_flag.store(true, Ordering::SeqCst);
                }
            },
        )?;
        assert_eq!(result, EmbeddingPassOutcome::Cancelled);
        assert_eq!(
            cancelled_progress.last(),
            Some(&i64::try_from(cancel_after)?)
        );
        assert!(!live.exists(), "cancellation published a vector prefix");

        worker.cancel_flag.store(false, Ordering::SeqCst);
        let mut retry_progress = Vec::new();
        assert_eq!(
            worker.generate_embeddings_from_source(
                &storage,
                &embedding_source::SliceEmbeddingSource(&messages),
                "hash",
                false,
                job_id,
                &index_path,
                &db_path,
                TierKind::Fast,
                &mut |completed| {
                    assert!(!live.exists(), "retry exposed an incomplete generation");
                    retry_progress.push(completed);
                },
            )?,
            EmbeddingPassOutcome::Completed
        );
        assert_eq!(retry_progress, [128, 256, 257]);
        let actual = VectorIndex::open_read_only(&live)?;
        assert_eq!(actual.record_count(), expected.record_count());
        assert_eq!(actual.wal_record_count(), 0);
        assert_eq!(actual.tombstone_count(), 0);
        for ordinal in 0..expected.record_count() {
            assert_eq!(actual.doc_id_at(ordinal)?, expected.doc_id_at(ordinal)?);
            let bits = |values: Vec<f32>| values.into_iter().map(f32::to_bits).collect::<Vec<_>>();
            assert_eq!(
                bits(actual.vector_at_f32(ordinal)?),
                bits(expected.vector_at_f32(ordinal)?)
            );
        }
        let jobs = storage.get_embedding_jobs(&db_path.to_string_lossy())?;
        assert_eq!((jobs[0].completed_docs, jobs[0].total_docs), (total, total));
    }
    Ok(())
}

#[test]
fn replacement_streaming_preserves_live_wal_until_complete_retry() -> anyhow::Result<()> {
    use frankensearch::index::wal_path_for;
    use std::collections::BTreeMap;

    let changed = EMBED_PROGRESS_CHUNK_SIZE * 2 + 1;
    let make_message = |ordinal: usize, edited: bool| MessageForEmbedding {
        message_id: i64::try_from(ordinal).unwrap(),
        created_at: Some(1_700_000_000_000),
        agent_id: 3,
        workspace_id: Some(5),
        source_id_hash: 7,
        role: "assistant".into(),
        content: format!(
            "{} compiler diagnostic document {ordinal} with independent evidence",
            if edited { "edited" } else { "original" }
        ),
    };
    let original: Vec<_> = (1..=changed + 2)
        .map(|ordinal| make_message(ordinal, false))
        .collect();
    let mut current: Vec<_> = (1..=changed)
        .map(|ordinal| make_message(ordinal, true))
        .collect();
    current.push(make_message(changed + 1, false));
    current.push(make_message(changed + 3, true));
    let inputs = |messages: &[MessageForEmbedding]| -> anyhow::Result<Vec<EmbeddingInput>> {
        let mut inputs = Vec::new();
        for message in messages {
            inputs.extend(
                canonical_message_inputs(message)?
                    .into_iter()
                    .map(|(_, input)| input),
            );
        }
        Ok(inputs)
    };
    let vector_bits = |index: &VectorIndex| -> anyhow::Result<BTreeMap<String, Vec<u32>>> {
        assert_eq!(index.wal_record_count(), 0);
        assert_eq!(index.tombstone_count(), 0);
        let mut result = BTreeMap::new();
        for row in 0..index.record_count() {
            assert!(
                result
                    .insert(
                        index.doc_id_at(row)?.to_owned(),
                        index
                            .vector_at_f32(row)?
                            .into_iter()
                            .map(f32::to_bits)
                            .collect(),
                    )
                    .is_none()
            );
        }
        Ok(result)
    };
    let indexer = SemanticIndexer::new("hash", None)?;
    let original_embedded = indexer.embed_messages(&inputs(&original)?)?;
    let mut expected_embedded = indexer.embed_messages(&inputs(&current)?)?;
    let retained_id = u64::try_from(changed + 1)?;
    // This valid WAL vector deliberately differs from a fresh embedding. Its
    // bits must survive reuse; merely re-embedding every current body is wrong.
    let mut wal_vector = original_embedded
        .iter()
        .find(|row| row.message_id == retained_id)
        .unwrap()
        .embedding
        .clone();
    for value in &mut wal_vector {
        *value = -*value;
    }
    expected_embedded
        .iter_mut()
        .find(|row| row.message_id == retained_id)
        .unwrap()
        .embedding = wal_vector.clone();
    let reference = tempfile::tempdir()?;
    let expected =
        vector_bits(&indexer.build_and_save_index(expected_embedded, reference.path())?)?;
    let wal_doc_id = canonical_message_inputs(&make_message(changed + 1, false))?[0]
        .0
        .clone();

    for cancel_after in [EMBED_PROGRESS_CHUNK_SIZE + 1, current.len()] {
        let temp = tempfile::tempdir()?;
        let db_path = temp.path().join("archive.db");
        let index_path = temp.path().join("semantic");
        let live = vector_index_path(&index_path, indexer.embedder_id());
        drop(indexer.build_and_save_index(original_embedded.clone(), &index_path)?);
        let mut writer = VectorIndex::open_writer(&live)?;
        writer.append_batch(&[(wal_doc_id.clone(), wal_vector.clone())])?;
        drop(writer);
        let wal_path = wal_path_for(&live);
        let before = std::fs::read(&live)?;
        let wal_before = std::fs::read(&wal_path)?;
        let storage = FrankenStorage::open(&db_path)?;
        let (worker, _) = EmbeddingWorker::new();
        let total = i64::try_from(current.len())?;
        let job_id = storage.upsert_embedding_job(&db_path.to_string_lossy(), "hash", total)?;
        storage.start_embedding_job(job_id)?;
        let mut progress = Vec::new();
        let result = worker.generate_embeddings_from_source(
            &storage,
            &embedding_source::SliceEmbeddingSource(&current),
            "hash",
            false,
            job_id,
            &index_path,
            &db_path,
            TierKind::Fast,
            &mut |completed| {
                assert_eq!(std::fs::read(&live).unwrap(), before);
                assert_eq!(std::fs::read(&wal_path).unwrap(), wal_before);
                progress.push(completed);
                if completed >= i64::try_from(cancel_after).unwrap() {
                    worker.cancel_flag.store(true, Ordering::SeqCst);
                }
            },
        )?;
        assert_eq!(result, EmbeddingPassOutcome::Cancelled);
        assert_eq!(progress.last(), Some(&i64::try_from(cancel_after)?));
        assert_eq!(std::fs::read(&live)?, before);
        assert_eq!(std::fs::read(&wal_path)?, wal_before);

        worker.cancel_flag.store(false, Ordering::SeqCst);
        let mut progress = Vec::new();
        assert_eq!(
            worker.generate_embeddings_from_source(
                &storage,
                &embedding_source::SliceEmbeddingSource(&current),
                "hash",
                false,
                job_id,
                &index_path,
                &db_path,
                TierKind::Fast,
                &mut |completed| {
                    assert_eq!(std::fs::read(&live).unwrap(), before);
                    assert_eq!(std::fs::read(&wal_path).unwrap(), wal_before);
                    progress.push(completed);
                },
            )?,
            EmbeddingPassOutcome::Completed
        );
        assert_eq!(progress, [129, 257, 259]);
        assert_eq!(vector_bits(&VectorIndex::open_read_only(&live)?)?, expected);
        let jobs = storage.get_embedding_jobs(&db_path.to_string_lossy())?;
        assert_eq!((jobs[0].completed_docs, jobs[0].total_docs), (total, total));
    }
    Ok(())
}
