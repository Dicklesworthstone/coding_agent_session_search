//! Public full rebuilds must preserve installed data until publication.
//! Real hash embeddings and FSVI/WAL writers; corruption is an explicit negative.
use anyhow::Result;
use coding_agent_search::indexer::semantic::{EmbeddedMessage, EmbeddingInput, SemanticIndexer};
use coding_agent_search::search::vector_index::{SemanticDocId, vector_index_path};
use frankensearch::index::{Quantization, VectorIndex, next_generation, wal_path_for};
use std::cell::Cell;
use std::fs;
use std::path::Path;

fn rows(indexer: &SemanticIndexer, start: u64) -> Result<Vec<EmbeddedMessage>> {
    indexer.embed_messages(&[
        EmbeddingInput::new(start, "compiler ownership checkpoint"),
        EmbeddingInput::new(start + 1, "network cancellation transcript"),
        EmbeddingInput::new(start + 2, "durable vector publication"),
    ])
}

fn doc_id(row: &EmbeddedMessage) -> String {
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

fn signature(index: &VectorIndex, query: &[f32]) -> Result<Vec<(String, u32)>> {
    let mut hits: Vec<_> = index
        .search_top_k(query, index.record_count() + index.wal_record_count() + 1, None)?
        .into_iter()
        .map(|hit| (hit.doc_id.to_string(), hit.score.to_bits()))
        .collect();
    hits.sort();
    Ok(hits)
}

fn reopened(path: &Path, query: &[f32]) -> Result<Vec<(String, u32)>> {
    signature(&VectorIndex::open_read_only(path)?, query)
}

fn write_generation(path: &Path, rows: &[EmbeddedMessage], generation: u8) -> Result<()> {
    let mut writer = VectorIndex::create_with_revision(
        path,
        "fnv1a-384",
        coding_agent_search::indexer::semantic::HASH_VECTOR_SPACE_REVISION,
        384,
        Quantization::F16,
    )?
    .with_generation(generation);
    for row in rows {
        writer.write_record(&doc_id(row), &row.embedding)?;
    }
    writer.finish()?;
    Ok(())
}

fn assert_no_scratch(parent: &Path) -> Result<()> {
    for entry in fs::read_dir(parent)? {
        assert!(
            !entry?.file_name().to_string_lossy().starts_with(".semantic-full-rebuild-"),
            "completed or rejected public rebuild left private scratch behind"
        );
    }
    Ok(())
}

#[test]
fn rejected_public_replacements_preserve_bytes_and_fresh_reader_results() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let original = rows(&indexer, 1)?;
    drop(indexer.build_and_save_index(original.clone(), temp.path())?);
    let path = vector_index_path(temp.path(), indexer.embedder_id());
    let before = fs::read(&path)?;
    let expected = reopened(&path, &original[0].embedding)?;
    for invalid in 0..5 {
        for position in 0..3 {
            let mut replacement = rows(&indexer, 20)?;
            match invalid {
                0 => { replacement[position].embedding.pop(); }
                1 => replacement[position].embedding[0] = f32::NAN,
                2 => replacement[position].embedding[0] = f32::INFINITY,
                3 => replacement[position].embedding[0] = f32::NEG_INFINITY,
                _ => replacement[position].embedding.fill(0.0),
            }
            let error = indexer.build_and_save_index(replacement, temp.path()).unwrap_err();
            assert_eq!(fs::read(&path)?, before, "invalid={invalid} position={position}");
            assert_eq!(reopened(&path, &original[0].embedding)?, expected);
            assert!(!wal_path_for(&path).exists());
            assert_no_scratch(path.parent().unwrap())?;
            if invalid == 4 {
                assert!(matches!(error.downcast_ref::<frankensearch::SearchError>(),
                    Some(frankensearch::SearchError::InvalidConfig { .. })));
            }
        }
    }
    eprintln!("public_rebuild_rejections cases=15 preserved=true");
    Ok(())
}

#[test]
fn valid_full_rebuild_refuses_acknowledged_wal_without_consuming_input() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let original = rows(&indexer, 1)?;
    let mut writer = indexer.build_and_save_index(original, temp.path())?;
    let pending = rows(&indexer, 90)?;
    writer.append_batch(&[(doc_id(&pending[0]), pending[0].embedding.clone())])?;
    assert_eq!(writer.wal_record_count(), 1);
    drop(writer);
    let path = vector_index_path(temp.path(), indexer.embedder_id());
    let wal = wal_path_for(&path);
    let before = (fs::read(&path)?, fs::read(&wal)?);
    let expected = reopened(&path, &pending[0].embedding)?;
    assert!(expected.iter().any(|hit| hit.0 == doc_id(&pending[0])));
    let consumed = Cell::new(0);
    let input = rows(&indexer, 20)?.into_iter().inspect(|_| consumed.set(consumed.get() + 1));
    let error = indexer.build_and_save_index(input, temp.path()).unwrap_err();
    assert_eq!(consumed.get(), 0);
    assert!(matches!(error.downcast_ref::<frankensearch::SearchError>(),
        Some(frankensearch::SearchError::InvalidConfig { field, .. }) if field == "wal_sidecar"));
    assert_eq!((fs::read(&path)?, fs::read(&wal)?), before);
    assert_eq!(reopened(&path, &pending[0].embedding)?, expected);
    eprintln!("public_rebuild_wal_refusal pending=1 preserved=true");
    Ok(())
}

#[test]
fn rejected_initial_build_never_installs_its_valid_prefix() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let mut input = rows(&indexer, 1)?;
    input[1].embedding[0] = f32::NAN;
    let path = vector_index_path(temp.path(), indexer.embedder_id());
    assert!(indexer.build_and_save_index(input, temp.path()).is_err());
    assert!(!path.exists());
    assert!(!wal_path_for(&path).exists());
    assert_no_scratch(path.parent().unwrap())?;
    Ok(())
}

#[test]
fn replacement_advances_generation_and_matches_an_independent_fresh_build() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let reference = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    drop(indexer.build_and_save_index(rows(&indexer, 1)?, temp.path())?);
    let path = vector_index_path(temp.path(), indexer.embedder_id());
    let prior_generation = VectorIndex::peek_compaction_gen(&path)?;
    let replacement = rows(&indexer, 20)?;
    let reference_index = indexer.build_and_save_index(replacement.clone(), reference.path())?;
    let result = indexer.build_and_save_index(replacement.clone(), temp.path())?;
    assert_eq!(VectorIndex::peek_compaction_gen(&path)?, next_generation(prior_generation));
    assert_eq!(result.path(), path);
    assert_eq!(result.wal_record_count(), 0);
    for query in &replacement {
        assert_eq!(signature(&result, &query.embedding)?, signature(&reference_index, &query.embedding)?);
    }
    assert_no_scratch(path.parent().unwrap())?;
    eprintln!("public_rebuild_publication reference_equal=true generation_advanced=true");
    Ok(())
}

#[cfg(unix)]
#[test]
fn a_retained_reader_keeps_its_original_generation_during_public_rebuild() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let original = rows(&indexer, 1)?;
    drop(indexer.build_and_save_index(original.clone(), temp.path())?);
    let path = vector_index_path(temp.path(), indexer.embedder_id());
    let retained = VectorIndex::open_read_only(&path)?;
    let expected = signature(&retained, &original[0].embedding)?;
    let replacement = indexer.build_and_save_index(rows(&indexer, 20)?, temp.path())?;
    assert_eq!(signature(&retained, &original[0].embedding)?, expected);
    assert_ne!(signature(&replacement, &original[0].embedding)?, expected);
    Ok(())
}

#[test]
fn generation_wrap_uses_the_native_nonzero_successor() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let path = vector_index_path(temp.path(), indexer.embedder_id());
    fs::create_dir_all(path.parent().unwrap())?;
    write_generation(&path, &rows(&indexer, 1)?, 255)?;
    drop(indexer.build_and_save_index(rows(&indexer, 20)?, temp.path())?);
    assert_eq!(VectorIndex::peek_compaction_gen(&path)?, 1);
    Ok(())
}

#[test]
fn empty_full_replacement_publishes_an_empty_successor() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    drop(indexer.build_and_save_index(rows(&indexer, 1)?, temp.path())?);
    let path = vector_index_path(temp.path(), indexer.embedder_id());
    let generation = VectorIndex::peek_compaction_gen(&path)?;
    let empty = indexer.build_and_save_index(Vec::<EmbeddedMessage>::new(), temp.path())?;
    assert_eq!(empty.record_count(), 0);
    assert_eq!(VectorIndex::peek_compaction_gen(&path)?, next_generation(generation));
    Ok(())
}

#[test]
fn f16_signal_loss_or_overflow_cannot_replace_a_usable_generation() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let control = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let original = rows(&indexer, 1)?;
    drop(indexer.build_and_save_index(original.clone(), temp.path())?);
    let path = vector_index_path(temp.path(), indexer.embedder_id());
    let before = fs::read(&path)?;
    for (name, value) in [("underflow", 1.0e-8_f32), ("overflow", 70_000.0_f32)] {
        let mut input = rows(&indexer, 20)?;
        input.truncate(1);
        input[0].embedding.fill(0.0);
        input[0].embedding[0] = value;
        // Real backend control: these inputs pass the f32 writer checks but
        // their persisted f16 representations cannot supply finite signal.
        let control_path = control.path().join(format!("{name}.fsvi"));
        write_generation(&control_path, &input, 1)?;
        assert!(!VectorIndex::open_read_only(&control_path)?.is_vector_usable(0));
        let error = indexer.build_and_save_index(input, temp.path()).unwrap_err();
        assert!(error.to_string().contains("unusable persisted semantic vector"), "{error:#}");
        assert_eq!(fs::read(&path)?, before);
        assert_eq!(reopened(&path, &original[0].embedding)?.len(), original.len());
    }
    Ok(())
}

#[test]
fn a_competing_publication_during_input_iteration_is_not_overwritten() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    drop(indexer.build_and_save_index(rows(&indexer, 1)?, temp.path())?);
    let path = vector_index_path(temp.path(), indexer.embedder_id());
    let competing_path = temp.path().join("competing.fsvi");
    write_generation(&competing_path, &rows(&indexer, 90)?,
        next_generation(VectorIndex::peek_compaction_gen(&path)?))?;
    let mut competing_bytes = None;
    let input = rows(&indexer, 20)?.into_iter().inspect(|_| {
        if competing_bytes.is_none() {
            drop(VectorIndex::install_replacement(&path, &competing_path).unwrap());
            competing_bytes = Some(fs::read(&path).unwrap());
        }
    });
    let error = indexer.build_and_save_index(input, temp.path()).unwrap_err();
    assert!(error.to_string().contains("destination changed"), "{error:#}");
    assert_eq!(fs::read(&path)?, competing_bytes.unwrap());
    assert_no_scratch(path.parent().unwrap())?;
    Ok(())
}

#[test]
fn wal_created_during_input_iteration_survives_publication_refusal() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    drop(indexer.build_and_save_index(rows(&indexer, 1)?, temp.path())?);
    let path = vector_index_path(temp.path(), indexer.embedder_id());
    let pending = rows(&indexer, 90)?;
    let mut acknowledged = None;
    let input = rows(&indexer, 20)?.into_iter().inspect(|_| {
        if acknowledged.is_none() {
            let mut writer = VectorIndex::open_writer(&path).unwrap();
            writer.append_batch(&[(doc_id(&pending[0]), pending[0].embedding.clone())]).unwrap();
            drop(writer);
            acknowledged = Some((fs::read(&path).unwrap(), fs::read(wal_path_for(&path)).unwrap()));
        }
    });
    assert!(indexer.build_and_save_index(input, temp.path()).is_err());
    assert_eq!((fs::read(&path)?, fs::read(wal_path_for(&path))?), acknowledged.unwrap());
    assert!(reopened(&path, &pending[0].embedding)?.iter().any(|hit| hit.0 == doc_id(&pending[0])));
    Ok(())
}

#[test]
fn an_orphan_wal_entry_is_not_a_clean_initial_build() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let path = vector_index_path(temp.path(), indexer.embedder_id());
    fs::create_dir_all(path.parent().unwrap())?;
    let wal = wal_path_for(&path);
    fs::write(&wal, b"")?;
    assert!(indexer.build_and_save_index(rows(&indexer, 1)?, temp.path()).is_err());
    assert!(!path.exists());
    assert_eq!(fs::read(&wal)?, Vec::<u8>::new());
    Ok(())
}

#[test]
fn a_corrupt_destination_is_preserved_and_not_reclassified_as_missing() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let path = vector_index_path(temp.path(), indexer.embedder_id());
    fs::create_dir_all(path.parent().unwrap())?;
    let corrupt = b"not a valid vector generation";
    fs::write(&path, corrupt)?;
    let consumed = Cell::new(0);
    let input = rows(&indexer, 1)?.into_iter().inspect(|_| consumed.set(consumed.get() + 1));
    assert!(indexer.build_and_save_index(input, temp.path()).is_err());
    assert_eq!(consumed.get(), 0);
    assert_eq!(fs::read(&path)?, corrupt);
    Ok(())
}

#[cfg(unix)]
#[test]
fn a_symlink_destination_does_not_authorize_replacing_its_target() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let target = temp.path().join("unrelated.fsvi");
    write_generation(&target, &rows(&indexer, 1)?, 1)?;
    let before = fs::read(&target)?;
    let path = vector_index_path(temp.path(), indexer.embedder_id());
    fs::create_dir_all(path.parent().unwrap())?;
    std::os::unix::fs::symlink(&target, &path)?;
    assert!(indexer.build_and_save_index(rows(&indexer, 20)?, temp.path()).is_err());
    assert!(fs::symlink_metadata(&path)?.file_type().is_symlink());
    assert_eq!(fs::read(&target)?, before);
    Ok(())
}

#[test]
fn recovery_parity_cannot_silently_remain_attached_to_a_replacement() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    drop(indexer.build_and_save_index(rows(&indexer, 1)?, temp.path())?);
    let path = vector_index_path(temp.path(), indexer.embedder_id());
    let before = fs::read(&path)?;
    let mut fec_name = path.as_os_str().to_os_string();
    fec_name.push(".fec");
    let fec = std::path::PathBuf::from(fec_name);
    // The policy checks ownership by presence, not a guessed parity format.
    // This entry is deliberately NOT claimed to be valid RaptorQ protection.
    fs::write(&fec, b"unadmitted recovery protection")?;
    let error = indexer.build_and_save_index(rows(&indexer, 20)?, temp.path()).unwrap_err();
    assert!(matches!(error.downcast_ref::<frankensearch::SearchError>(),
        Some(frankensearch::SearchError::InvalidConfig { field, .. }) if field == "fec_sidecar"));
    assert_eq!(fs::read(&path)?, before);
    assert_eq!(fs::read(&fec)?, b"unadmitted recovery protection");
    Ok(())
}
