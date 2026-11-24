//! Exercise the worker's real hash embedder, private FSVI writer, and durable
//! progress writes across cancellation after a complete batch and the tail.

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
