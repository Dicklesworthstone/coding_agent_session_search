use super::*;
use crate::indexer::semantic::EmbeddedMessage;
use crate::search::vector_index::SemanticDocId;
use crate::storage::sqlite::MessageForEmbedding;
use frankensearch::index::{Quantization, next_generation};
use std::cell::Cell;
use std::collections::BTreeMap;

fn message(id: i64, content: &str) -> MessageForEmbedding {
    MessageForEmbedding {
        message_id: id,
        created_at: Some(1_700_000_000_000),
        agent_id: 7,
        workspace_id: Some(9),
        source_id_hash: 11,
        role: "assistant".into(),
        content: content.into(),
    }
}

fn row(message: &MessageForEmbedding) -> anyhow::Result<EmbeddedMessage> {
    let indexer = SemanticIndexer::new("hash", None)?;
    let inputs: Vec<_> = canonical_message_inputs(message)?
        .into_iter()
        .map(|(_, input)| input)
        .collect();
    let mut rows = indexer.embed_messages(&inputs)?;
    assert_eq!(rows.len(), 1);
    Ok(rows.remove(0))
}

fn id(row: &EmbeddedMessage) -> String {
    SemanticDocId {
        message_id: row.message_id,
        chunk_idx: row.chunk_idx,
        agent_id: row.agent_id,
        workspace_id: row.workspace_id,
        source_id: row.source_id,
        role: row.role,
        created_at_ms: row.created_at_ms,
        content_hash: Some(row.content_hash),
    }
    .to_doc_id_string()
}

fn write(
    path: &Path,
    rows: &[EmbeddedMessage],
    quantization: Quantization,
    revision: &str,
    generation: u8,
) -> anyhow::Result<()> {
    fs::create_dir_all(path.parent().unwrap())?;
    let mut writer =
        VectorIndex::create_with_revision(path, "fnv1a-384", revision, 384, quantization)?
            .with_generation(generation);
    for row in rows {
        writer.write_record(&id(row), &row.embedding)?;
    }
    writer.finish()?;
    Ok(())
}

fn revision() -> &'static str {
    expected_vector_space_revision("fnv1a-384").unwrap()
}

fn open(dir: &Path) -> anyhow::Result<ExistingIndexState> {
    ExistingIndexState::open(dir, &WorkerEmbedderKind::Hash, &|| false)?
        .context("unexpected cancellation")
}

fn bits(index: &VectorIndex) -> anyhow::Result<BTreeMap<String, Vec<u32>>> {
    let mut result = BTreeMap::new();
    for ordinal in 0..index.record_count() {
        if !index.is_deleted(ordinal) {
            result.insert(
                index.doc_id_at(ordinal)?.to_owned(),
                index
                    .vector_at_f32(ordinal)?
                    .into_iter()
                    .map(f32::to_bits)
                    .collect(),
            );
        }
    }
    for (id, vector) in index.wal_records() {
        result.insert(id.to_owned(), vector.iter().map(|v| v.to_bits()).collect());
    }
    Ok(result)
}

#[test]
fn reuse_planning_coexists_with_readers_and_keeps_a_shared_owner() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let path = vector_index_path(temp.path(), "fnv1a-384");
    let rows = vec![row(&message(1, "retained compiler diagnostic"))?];
    write(&path, &rows, Quantization::F16, revision(), 17)?;
    let before = fs::read(&path)?;
    let old_reader = VectorIndex::open_read_only(&path)?;
    let state = open(temp.path())?;
    assert!(state.exactly_matches(&HashSet::from([id(&rows[0])])));
    assert_eq!(state.active_count(&id(&rows[0])), 1);
    drop(old_reader);
    let next_reader = VectorIndex::open_read_only(&path)?;
    assert_eq!(bits(&next_reader)?, bits(state.source.as_ref().unwrap())?);
    assert!(VectorIndex::open_writer(&path).is_err());
    assert_eq!(fs::read(&path)?, before);
    drop(next_reader);
    drop(state);
    drop(VectorIndex::open_writer(&path)?);
    Ok(())
}

#[test]
fn writer_conflict_corruption_and_orphan_logs_are_not_missing_coverage() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let path = vector_index_path(temp.path(), "fnv1a-384");
    let rows = vec![row(&message(1, "writer locked source"))?];
    write(&path, &rows, Quantization::F16, revision(), 1)?;
    let before = fs::read(&path)?;
    let writer = VectorIndex::open_writer(&path)?;
    assert!(open(temp.path()).is_err());
    assert_eq!(fs::read(&path)?, before);
    drop(writer);

    let corrupt = tempfile::tempdir()?;
    let broken = vector_index_path(corrupt.path(), "fnv1a-384");
    fs::create_dir_all(broken.parent().unwrap())?;
    fs::write(&broken, b"explicit corrupt fixture")?;
    assert!(open(corrupt.path()).is_err());
    assert_eq!(fs::read(&broken)?, b"explicit corrupt fixture");

    let missing = tempfile::tempdir()?;
    let absent = vector_index_path(missing.path(), "fnv1a-384");
    let state = open(missing.path())?;
    assert!(!state.path_exists);
    fs::create_dir_all(absent.parent().unwrap())?;
    fs::write(wal_path_for(&absent), b"orphan fixture")?;
    assert!(state.ensure_current().is_err());
    assert!(
        open(missing.path())
            .unwrap_err()
            .to_string()
            .contains("orphan WAL")
    );
    assert_eq!(fs::read(wal_path_for(&absent))?, b"orphan fixture");
    Ok(())
}

#[test]
fn wal_supersedes_main_before_cleanup_and_invalid_wal_never_revives_main() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let path = vector_index_path(temp.path(), "fnv1a-384");
    let original = row(&message(1, "same canonical identity"))?;
    write(
        &path,
        std::slice::from_ref(&original),
        Quantization::F16,
        revision(),
        17,
    )?;
    let producer = temp.path().join("producer.fsvi");
    fs::copy(&path, &producer)?;
    let mut writer = VectorIndex::open_writer(&producer)?;
    // Valid F32 input loses all stored F16 signal. The legacy native appender
    // can produce it; admission must not reuse it or fall back to old main.
    writer.append_batch(&[(id(&original), vec![1e-20; 384])])?;
    drop(writer);
    fs::copy(wal_path_for(&producer), wal_path_for(&path))?;
    let before = (fs::read(&path)?, fs::read(wal_path_for(&path))?);
    let state = open(temp.path())?;
    assert_eq!(state.source.as_ref().unwrap().tombstone_count(), 0);
    assert_eq!(state.source.as_ref().unwrap().wal_record_count(), 1);
    assert_eq!(state.active_count(&id(&original)), 0);
    assert!(!state.exactly_matches(&HashSet::from([id(&original)])));
    assert_eq!((fs::read(&path)?, fs::read(wal_path_for(&path))?), before);
    Ok(())
}

#[test]
fn stale_and_torn_wal_observation_never_mutates_the_log() -> anyhow::Result<()> {
    use std::io::Write;
    let temp = tempfile::tempdir()?;
    let path = vector_index_path(temp.path(), "fnv1a-384");
    let original = row(&message(1, "main identity"))?;
    write(
        &path,
        std::slice::from_ref(&original),
        Quantization::F16,
        revision(),
        17,
    )?;
    let mut writer = VectorIndex::open_writer(&path)?;
    let pending = row(&message(2, "pending identity"))?;
    writer.append_batch(&[(id(&pending), pending.embedding.clone())])?;
    drop(writer);
    fs::OpenOptions::new()
        .append(true)
        .open(wal_path_for(&path))?
        .write_all(b"torn incomplete trailer")?;
    let before = fs::read(wal_path_for(&path))?;
    let state = open(temp.path())?;
    assert_eq!(state.active_count(&id(&pending)), 1);
    assert_eq!(fs::read(wal_path_for(&path))?, before);
    drop(state);

    let newer = temp.path().join("newer.fsvi");
    write(
        &newer,
        std::slice::from_ref(&original),
        Quantization::F16,
        revision(),
        next_generation(17),
    )?;
    fs::rename(&newer, &path)?;
    let state = open(temp.path())?;
    assert_eq!(state.active_count(&id(&pending)), 0);
    assert!(!state.exactly_matches(&HashSet::from([id(&original)])));
    assert_eq!(fs::read(wal_path_for(&path))?, before);
    Ok(())
}

#[test]
fn unusable_duplicate_and_foreign_rows_cannot_certify_an_unchanged_index() -> anyhow::Result<()> {
    let original = row(&message(1, "one canonical record"))?;
    let current = HashSet::from([id(&original)]);
    for case in 0..3 {
        let temp = tempfile::tempdir()?;
        let path = vector_index_path(temp.path(), "fnv1a-384");
        let mut rows = vec![original.clone()];
        let mut space = revision();
        match case {
            0 => rows[0].embedding.fill(1e-20),
            1 => rows.push(original.clone()),
            _ => space = "foreign-producer-contract",
        }
        write(&path, &rows, Quantization::F16, space, 1)?;
        let before = fs::read(&path)?;
        let state = open(temp.path())?;
        assert_ne!(state.active_count(&id(&original)), 1);
        assert!(!state.exactly_matches(&current));
        assert_eq!(fs::read(&path)?, before);
    }
    Ok(())
}

#[test]
fn generation_replacement_and_cancelled_census_never_authorize_reuse() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let path = vector_index_path(temp.path(), "fnv1a-384");
    let rows = vec![row(&message(1, "generation identity"))?];
    write(&path, &rows, Quantization::F16, revision(), 17)?;
    let before = fs::read(&path)?;
    let calls = Cell::new(0usize);
    let cancelled = || {
        calls.set(calls.get() + 1);
        calls.get() == 3
    };
    assert!(
        ExistingIndexState::open(temp.path(), &WorkerEmbedderKind::Hash, &cancelled)?.is_none()
    );
    assert_eq!(fs::read(&path)?, before);
    let state = open(temp.path())?;
    let replacement = temp.path().join("replacement.fsvi");
    write(&replacement, &rows, Quantization::F16, revision(), 17)?;
    fs::rename(&replacement, &path)?;
    assert!(
        state.ensure_current().is_err(),
        "same generation number is not same ownership"
    );
    assert_eq!(
        bits(state.source.as_ref().unwrap())?,
        bits(&VectorIndex::open_read_only(&path)?)?
    );
    Ok(())
}

#[test]
fn real_daemon_delta_serves_retained_readers_and_preserves_acknowledged_wal_vectors()
-> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let db = temp.path().join("archive.db");
    let directory = temp.path().join("semantic");
    let storage = FrankenStorage::open(&db)?;
    let job = storage.upsert_embedding_job(&db.to_string_lossy(), "hash", 2)?;
    storage.start_embedding_job(job)?;
    let (worker, _) = EmbeddingWorker::new();
    let first = message(1, "retained main compiler record");
    let second = message(2, "acknowledged pending compiler record");
    let path = vector_index_path(&directory, "fnv1a-384");
    let original = row(&first)?;
    write(&path, &[original], Quantization::F16, revision(), 1)?;
    let mut pending = row(&second)?;
    for value in &mut pending.embedding {
        *value = -*value;
    }
    let mut writer = VectorIndex::open_writer(&path)?;
    writer.append_batch(&[(id(&pending), pending.embedding)])?;
    drop(writer);
    let retained = VectorIndex::open_read_only(&path)?;
    let expected = bits(&retained)?;
    assert_eq!(
        worker.generate_embeddings_and_save(
            &storage,
            &[first, second],
            "hash",
            false,
            job,
            &directory,
            &db,
        )?,
        EmbeddingPassOutcome::Completed
    );
    let fresh = VectorIndex::open_read_only(&path)?;
    assert_eq!(fresh.wal_record_count(), 0);
    assert_eq!(
        bits(&fresh)?,
        expected,
        "WAL vector must be retained, not needlessly re-embedded"
    );
    assert_eq!(bits(&retained)?, expected);
    Ok(())
}

#[test]
fn real_daemon_repairs_stored_zero_signal_instead_of_reporting_unchanged() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let db = temp.path().join("archive.db");
    let directory = temp.path().join("semantic");
    let storage = FrankenStorage::open(&db)?;
    let job = storage.upsert_embedding_job(&db.to_string_lossy(), "hash", 1)?;
    storage.start_embedding_job(job)?;
    let (worker, _) = EmbeddingWorker::new();
    let message = message(1, "restore this canonical vector from its real text");
    let original = row(&message)?;
    let mut unusable = original.clone();
    unusable.embedding.fill(1e-20);
    let path = vector_index_path(&directory, "fnv1a-384");
    write(&path, &[unusable], Quantization::F16, revision(), 1)?;
    let retained = VectorIndex::open_read_only(&path)?;
    assert!(!retained.is_vector_usable(0));
    assert_eq!(
        worker.generate_embeddings_and_save(
            &storage,
            &[message],
            "hash",
            false,
            job,
            &directory,
            &db,
        )?,
        EmbeddingPassOutcome::Completed
    );
    let oracle_path = temp.path().join("oracle.fsvi");
    write(&oracle_path, &[original], Quantization::F16, revision(), 1)?;
    let fresh = VectorIndex::open_read_only(&path)?;
    assert!(fresh.is_vector_usable(0));
    assert_eq!(
        bits(&fresh)?,
        bits(&VectorIndex::open_read_only(&oracle_path)?)?
    );
    assert!(!retained.is_vector_usable(0));
    Ok(())
}

#[test]
fn real_daemon_unchanged_pass_keeps_the_original_generation_with_a_live_reader() -> anyhow::Result<()>
{
    let temp = tempfile::tempdir()?;
    let db = temp.path().join("archive.db");
    let directory = temp.path().join("semantic");
    let storage = FrankenStorage::open(&db)?;
    let job = storage.upsert_embedding_job(&db.to_string_lossy(), "hash", 1)?;
    storage.start_embedding_job(job)?;
    let (worker, _) = EmbeddingWorker::new();
    let message = message(1, "unchanged searchable compiler history");
    let path = vector_index_path(&directory, "fnv1a-384");
    write(&path, &[row(&message)?], Quantization::F16, revision(), 17)?;
    let reader = VectorIndex::open_read_only(&path)?;
    let before = SourceFiles::capture(&path)?;
    let bytes = fs::read(&path)?;
    assert_eq!(
        worker.generate_embeddings_and_save(
            &storage,
            &[message],
            "hash",
            false,
            job,
            &directory,
            &db,
        )?,
        EmbeddingPassOutcome::Completed
    );
    assert_eq!(SourceFiles::capture(&path)?, before);
    assert_eq!(fs::read(&path)?, bytes);
    assert_eq!(bits(&reader)?, bits(&VectorIndex::open_read_only(&path)?)?);
    Ok(())
}
