//! Public full rebuilds must preserve installed data until publication.
//! Real hash embeddings and FSVI/WAL writers; corruption is an explicit negative.
use anyhow::Result;
use coding_agent_search::indexer::semantic::{EmbeddedMessage, EmbeddingInput, SemanticIndexer};
use coding_agent_search::search::vector_index::{SemanticDocId, vector_index_path};
use coding_agent_search::search::semantic_manifest::TierKind;
use frankensearch::index::{Quantization, VectorIndex, next_generation, wal_path_for};
use std::cell::Cell;
use std::collections::{BTreeMap, HashSet};
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
#[cfg(unix)]
fn same_generation_publication_is_not_overwritten_by_a_full_rebuild() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    drop(indexer.build_and_save_index(rows(&indexer, 1)?, temp.path())?);
    let path = vector_index_path(temp.path(), indexer.embedder_id());
    let generation = VectorIndex::peek_compaction_gen(&path)?;
    let concurrent = temp.path().join("concurrent.fsvi");
    write_generation(&concurrent, &rows(&indexer, 70)?, generation)?;
    let expected = fs::read(&concurrent)?;
    let published = Cell::new(false);
    let input = rows(&indexer, 20)?.into_iter().inspect(|_| {
        if !published.replace(true) {
            fs::rename(&concurrent, &path).unwrap();
        }
    });
    let error = indexer.build_and_save_index(input, temp.path()).unwrap_err();
    assert!(error.to_string().contains("destination changed"), "{error:#}");
    assert!(published.get());
    assert_eq!(VectorIndex::peek_compaction_gen(&path)?, generation);
    assert_eq!(fs::read(&path)?, expected);
    assert_no_scratch(path.parent().unwrap())?;
    Ok(())
}

#[test]
fn same_generation_in_place_change_is_not_overwritten_by_a_full_rebuild() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    drop(indexer.build_and_save_index(rows(&indexer, 1)?, temp.path())?);
    let path = vector_index_path(temp.path(), indexer.embedder_id());
    let generation = VectorIndex::peek_compaction_gen(&path)?;
    let concurrent = temp.path().join("concurrent.fsvi");
    write_generation(&concurrent, &rows(&indexer, 4)?, generation)?;
    let expected = fs::read(&concurrent)?;
    assert_eq!(expected.len(), fs::read(&path)?.len());
    let changed = Cell::new(false);
    let input = rows(&indexer, 20)?.into_iter().inspect(|_| {
        if !changed.replace(true) {
            fs::write(&path, &expected).unwrap();
            let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
            file.set_times(fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH)).unwrap();
        }
    });
    let error = indexer.build_and_save_index(input, temp.path()).unwrap_err();
    assert!(error.to_string().contains("destination changed"), "{error:#}");
    assert!(changed.get());
    assert_eq!(VectorIndex::peek_compaction_gen(&path)?, generation);
    assert_eq!(fs::read(&path)?, expected);
    assert_no_scratch(path.parent().unwrap())?;
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

fn canonical_ids(rows: &[EmbeddedMessage]) -> HashSet<String> {
    rows.iter().map(doc_id).collect()
}

fn vector_bits(index: &VectorIndex) -> Result<BTreeMap<String, Vec<u32>>> {
    let mut actual = BTreeMap::new();
    for row in 0..index.record_count() {
        assert!(!index.is_deleted(row));
        assert!(actual.insert(index.doc_id_at(row)?.to_owned(),
            index.vector_at_f32(row)?.into_iter().map(f32::to_bits).collect()).is_none());
    }
    assert_eq!(index.wal_record_count(), 0);
    Ok(actual)
}

fn retained_log(path: &Path, rows: &[EmbeddedMessage]) -> Result<()> {
    // Genuine log-before-main-cleanup state: the sibling writer serializes the
    // log, while the original main remains unchanged. No header/CRC patching.
    let producer = path.with_extension("producer.fsvi");
    fs::copy(path, &producer)?;
    let mut source = VectorIndex::open_writer(&producer)?;
    source.append_batch(&rows.iter().map(|row| (doc_id(row), row.embedding.clone())).collect::<Vec<_>>())?;
    assert_eq!(source.wal_record_count(), rows.len());
    drop(source);
    fs::copy(wal_path_for(&producer), wal_path_for(path))?;
    Ok(())
}

#[test]
fn canonical_full_replacement_supersedes_retained_wal_at_generation_wrap() -> Result<()> {
    for generation in [1, 254, 255] {
        let temp = tempfile::tempdir()?;
        let indexer = SemanticIndexer::new("hash", None)?;
        let path = vector_index_path(temp.path(), indexer.embedder_id());
        fs::create_dir_all(path.parent().unwrap())?;
        let original = rows(&indexer, 1)?;
        write_generation(&path, &original, generation)?;
        retained_log(&path, &rows(&indexer, 90)?[..1])?;
        let retained = VectorIndex::open_read_only(&path)?;
        let before = signature(&retained, &original[0].embedding)?;
        assert_eq!(retained.wal_record_count(), 1);
        let replacements = rows(&indexer, 20)?;
        let oracle_root = tempfile::tempdir()?;
        let oracle = indexer.build_and_save_index(replacements.clone(), oracle_root.path())?;
        let actual = indexer.reconcile_index_with_canonical_documents(
            replacements.clone(), temp.path(), TierKind::Fast, "canonical-complete", &canonical_ids(&replacements),
        )?;
        assert_eq!(VectorIndex::peek_compaction_gen(&path)?, next_generation(generation));
        assert_eq!(vector_bits(&actual)?, vector_bits(&oracle)?);
        assert_eq!(signature(&retained, &original[0].embedding)?, before);
        assert!(!wal_path_for(&path).exists());
        drop(actual);
        assert_eq!(vector_bits(&VectorIndex::open_read_only(&path)?)?, vector_bits(&oracle)?);
    }
    Ok(())
}

#[test]
fn canonical_delta_preserves_latest_wal_and_unchanged_vectors_without_resurrection() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let original = rows(&indexer, 1)?;
    drop(indexer.build_and_save_index(original.clone(), temp.path())?);
    let path = vector_index_path(temp.path(), indexer.embedder_id());
    let mut replacement_in_wal = original[1].clone();
    // Same source identity, different acknowledged vector: the log must win
    // even though best-effort main tombstoning has not happened in this image.
    for value in &mut replacement_in_wal.embedding { *value = -*value; }
    let added_in_wal = rows(&indexer, 90)?[0].clone();
    retained_log(&path, &[replacement_in_wal.clone(), added_in_wal.clone()])?;
    let retained = VectorIndex::open_read_only(&path)?;
    assert_eq!(retained.tombstone_count(), 0);
    assert_eq!(retained.wal_record_count(), 2);
    let edited = indexer.embed_messages(&[EmbeddingInput::new(1, "edited canonical compiler message")])?;
    let expected_rows = vec![edited[0].clone(), replacement_in_wal, added_in_wal];
    let oracle_root = tempfile::tempdir()?;
    let oracle = indexer.build_and_save_index(expected_rows.clone(), oracle_root.path())?;
    let actual = indexer.reconcile_index_with_canonical_documents(
        edited, temp.path(), TierKind::Fast, "canonical-delta", &canonical_ids(&expected_rows),
    )?;
    assert_eq!(vector_bits(&actual)?, vector_bits(&oracle)?);
    assert!(!vector_bits(&actual)?.contains_key(&doc_id(&original[0])));
    assert!(!vector_bits(&actual)?.contains_key(&doc_id(&original[2])));
    assert_eq!(retained.record_count(), 3);
    assert_eq!(retained.wal_record_count(), 2);
    Ok(())
}

#[test]
fn canonical_missing_coverage_and_invalid_deltas_preserve_main_and_wal_bytes() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let original = rows(&indexer, 1)?;
    drop(indexer.build_and_save_index(original.clone(), temp.path())?);
    let path = vector_index_path(temp.path(), indexer.embedder_id());
    retained_log(&path, &rows(&indexer, 90)?[..1])?;
    let before = (fs::read(&path)?, fs::read(wal_path_for(&path))?);
    let expected_hits = reopened(&path, &original[0].embedding)?;
    let missing = rows(&indexer, 80)?[0].clone();
    let mut ids = canonical_ids(&original);
    ids.insert(doc_id(&missing));
    let error = indexer.reconcile_index_with_canonical_documents(
        Vec::new(), temp.path(), TierKind::Fast, "missing", &ids,
    ).unwrap_err();
    assert!(error.to_string().contains("lacks vectors"), "{error:#}");
    for kind in 0..5 {
        let mut delta = vec![missing.clone()];
        match kind {
            0 => { delta[0].embedding.pop(); }
            1 => delta[0].embedding[0] = f32::NAN,
            2 => delta[0].embedding.fill(0.0),
            3 => delta.push(missing.clone()),
            _ => delta = vec![rows(&indexer, 800)?[0].clone()],
        }
        assert!(indexer.reconcile_index_with_canonical_documents(
            delta, temp.path(), TierKind::Fast, "invalid", &ids,
        ).is_err());
        assert_eq!((fs::read(&path)?, fs::read(wal_path_for(&path))?), before);
        assert_eq!(reopened(&path, &original[0].embedding)?, expected_hits);
    }
    assert_eq!((fs::read(&path)?, fs::read(wal_path_for(&path))?), before);
    Ok(())
}

#[test]
fn canonical_persisted_signal_loss_cannot_replace_a_usable_main_and_wal() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    drop(indexer.build_and_save_index(rows(&indexer, 1)?, temp.path())?);
    let path = vector_index_path(temp.path(), indexer.embedder_id());
    retained_log(&path, &rows(&indexer, 90)?[..1])?;
    let before = (fs::read(&path)?, fs::read(wal_path_for(&path))?);
    for value in [1e-20f32, 70_000.0f32] {
        let mut replacement = rows(&indexer, 20)?;
        replacement[1].embedding.fill(value);
        let ids = canonical_ids(&replacement);
        let error = indexer.reconcile_index_with_canonical_documents(
            replacement, temp.path(), TierKind::Fast, "stored-signal", &ids,
        ).unwrap_err();
        assert!(error.to_string().contains("unusable persisted"), "{error:#}");
        assert_eq!((fs::read(&path)?, fs::read(wal_path_for(&path))?), before);
    }
    Ok(())
}

#[test]
fn canonical_partial_reuse_preserves_full_precision_storage() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let path = vector_index_path(temp.path(), indexer.embedder_id());
    fs::create_dir_all(path.parent().unwrap())?;
    let mut original = rows(&indexer, 1)?;
    original[0].embedding[0] = 0.123_456_79;
    let mut writer = VectorIndex::create_with_revision(&path, "fnv1a-384",
        coding_agent_search::indexer::semantic::HASH_VECTOR_SPACE_REVISION, 384, Quantization::F32)?;
    for row in &original { writer.write_record(&doc_id(row), &row.embedding)?; }
    writer.finish()?;
    let mut pending = rows(&indexer, 90)?[0].clone();
    pending.embedding[0] = 0.987_654_3;
    retained_log(&path, &[pending.clone()])?;
    let added = rows(&indexer, 20)?[0].clone();
    let expected_rows = vec![original[0].clone(), pending, added.clone()];
    let actual = indexer.reconcile_index_with_canonical_documents(
        vec![added], temp.path(), TierKind::Fast, "f32-retention", &canonical_ids(&expected_rows),
    )?;
    assert_eq!(actual.quantization(), Quantization::F32);
    let expected = expected_rows.iter().map(|row| (doc_id(row),
        row.embedding.iter().map(|value| value.to_bits()).collect::<Vec<_>>())).collect::<BTreeMap<_, _>>();
    assert_eq!(vector_bits(&actual)?, expected);
    Ok(())
}

#[test]
fn canonical_foreign_vector_space_requires_all_replacement_vectors() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let path = vector_index_path(temp.path(), indexer.embedder_id());
    fs::create_dir_all(path.parent().unwrap())?;
    let original = rows(&indexer, 1)?;
    let mut writer = VectorIndex::create_with_revision(&path, "fnv1a-384", "foreign-revision", 384, Quantization::F16)?;
    for row in &original { writer.write_record(&doc_id(row), &row.embedding)?; }
    writer.finish()?;
    retained_log(&path, &rows(&indexer, 90)?[..1])?;
    let before = (fs::read(&path)?, fs::read(wal_path_for(&path))?);
    let ids = canonical_ids(&original);
    assert!(indexer.reconcile_index_with_canonical_documents(
        vec![original[0].clone()], temp.path(), TierKind::Fast, "foreign", &ids,
    ).is_err());
    assert_eq!((fs::read(&path)?, fs::read(wal_path_for(&path))?), before);
    let actual = indexer.reconcile_index_with_canonical_documents(
        original.clone(), temp.path(), TierKind::Fast, "foreign", &ids,
    )?;
    assert_eq!(actual.embedder_revision(), coding_agent_search::indexer::semantic::HASH_VECTOR_SPACE_REVISION);
    let oracle_root = tempfile::tempdir()?;
    let oracle = indexer.build_and_save_index(original, oracle_root.path())?;
    assert_eq!(vector_bits(&actual)?, vector_bits(&oracle)?);
    Ok(())
}

#[test]
fn canonical_empty_set_retires_main_and_wal_membership() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    drop(indexer.build_and_save_index(rows(&indexer, 1)?, temp.path())?);
    let path = vector_index_path(temp.path(), indexer.embedder_id());
    retained_log(&path, &rows(&indexer, 90)?[..1])?;
    let actual = indexer.reconcile_index_with_canonical_documents(
        Vec::new(), temp.path(), TierKind::Fast, "deleted-all", &HashSet::new(),
    )?;
    assert_eq!(actual.record_count(), 0);
    assert_eq!(actual.wal_record_count(), 0);
    assert!(!wal_path_for(&path).exists());
    Ok(())
}

#[test]
fn canonical_reconciliation_refuses_a_live_writer_and_unowned_recovery_parity() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let original = rows(&indexer, 1)?;
    drop(indexer.build_and_save_index(original.clone(), temp.path())?);
    let path = vector_index_path(temp.path(), indexer.embedder_id());
    retained_log(&path, &rows(&indexer, 90)?[..1])?;
    let source_writer = VectorIndex::open_writer(&path)?;
    let before = (fs::read(&path)?, fs::read(wal_path_for(&path))?);
    assert!(indexer.reconcile_index_with_canonical_documents(
        Vec::new(), temp.path(), TierKind::Fast, "writer-conflict", &canonical_ids(&original),
    ).is_err());
    assert_eq!((fs::read(&path)?, fs::read(wal_path_for(&path))?), before);
    drop(source_writer);
    let mut fec_path = path.as_os_str().to_os_string();
    fec_path.push(".fec");
    let fec_path = std::path::PathBuf::from(fec_path);
    fs::write(&fec_path, b"unadmitted parity")?;
    let error = indexer.reconcile_index_with_canonical_documents(
        Vec::new(), temp.path(), TierKind::Fast, "parity-conflict", &canonical_ids(&original),
    ).unwrap_err();
    assert!(matches!(error.downcast_ref::<frankensearch::SearchError>(),
        Some(frankensearch::SearchError::InvalidConfig { field, .. }) if field == "fec_sidecar"));
    assert_eq!((fs::read(&path)?, fs::read(wal_path_for(&path))?), before);
    assert_eq!(fs::read(&fec_path)?, b"unadmitted parity");
    Ok(())
}

#[test]
fn canonical_unchanged_reconciliation_retains_the_same_inode_and_shared_readers() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let original = rows(&indexer, 1)?;
    let path = vector_index_path(temp.path(), indexer.embedder_id());
    fs::create_dir_all(path.parent().unwrap())?;
    write_generation(&path, &original, 254)?;
    let owner = same_file::Handle::from_path(&path)?;
    let before = fs::read(&path)?;
    let modified = owner.as_file().metadata()?.modified()?;
    let retained = VectorIndex::open_read_only(&path)?;
    let expected = signature(&retained, &original[0].embedding)?;
    for _ in 0..3 {
        let unchanged = indexer.reconcile_index_with_canonical_documents(
            Vec::new(), temp.path(), TierKind::Fast, "unchanged-current", &canonical_ids(&original),
        )?;
        assert_eq!(same_file::Handle::from_path(&path)?, owner,
            "an unchanged reconciliation must not install a copied inode");
        assert_eq!(fs::metadata(&path)?.modified()?, modified);
        assert_eq!(fs::read(&path)?, before);
        assert_eq!(VectorIndex::peek_compaction_gen(&path)?, 254);
        assert_eq!(signature(&unchanged, &original[0].embedding)?, expected);
        // A retained generation is a query owner, not an exclusive writer
        // which prevents every new query until its result handle is dropped.
        let fresh = VectorIndex::open_read_only(&path)?;
        assert_eq!(signature(&fresh, &original[0].embedding)?, expected);
        assert_eq!(signature(&retained, &original[0].embedding)?, expected);
    }
    Ok(())
}

#[test]
fn canonical_unchanged_reconciliation_preserves_recovery_parity_without_retiring_it() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let original = rows(&indexer, 1)?;
    drop(indexer.build_and_save_index(original.clone(), temp.path())?);
    let path = vector_index_path(temp.path(), indexer.embedder_id());
    let owner = same_file::Handle::from_path(&path)?;
    let before = fs::read(&path)?;
    let mut name = path.as_os_str().to_os_string();
    name.push(".fec");
    let fec = std::path::PathBuf::from(name);
    // Presence stands for protection whose format/ownership this API does
    // not interpret. A no-op may retain it, but must never authorize removal.
    fs::write(&fec, b"opaque owner-managed parity")?;
    let ids = canonical_ids(&original);
    let unchanged = indexer.reconcile_index_with_canonical_documents(
        Vec::new(), temp.path(), TierKind::Fast, "unchanged-parity", &ids,
    )?;
    assert_eq!(same_file::Handle::from_path(&path)?, owner);
    assert_eq!(vector_bits(&unchanged)?.len(), original.len());
    assert_eq!(fs::read(&path)?, before);
    assert_eq!(fs::read(&fec)?, b"opaque owner-managed parity");
    drop(unchanged);
    // A deletion also has an empty embedding delta, but is not unchanged.
    let mut reduced = ids;
    reduced.remove(&doc_id(&original[1]));
    let error = indexer.reconcile_index_with_canonical_documents(
        Vec::new(), temp.path(), TierKind::Fast, "deleted-parity", &reduced,
    ).unwrap_err();
    assert!(matches!(error.downcast_ref::<frankensearch::SearchError>(),
        Some(frankensearch::SearchError::InvalidConfig { field, .. }) if field == "fec_sidecar"));
    assert_eq!(same_file::Handle::from_path(&path)?, owner);
    assert_eq!(fs::read(&path)?, before);
    assert_eq!(fs::read(&fec)?, b"opaque owner-managed parity");
    Ok(())
}

#[test]
fn canonical_empty_delta_does_not_skip_same_count_identity_changes_or_missing_coverage() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let original = rows(&indexer, 1)?;
    drop(indexer.build_and_save_index(original.clone(), temp.path())?);
    let path = vector_index_path(temp.path(), indexer.embedder_id());
    let before = fs::read(&path)?;
    for variant in 0..5 {
        let mut current = original.clone();
        match variant {
            0 => current[1].content_hash[0] ^= 1,
            1 => current[1].role ^= 1,
            2 => current[1].agent_id += 1,
            3 => current[1].source_id += 1,
            _ => current[1].workspace_id += 1,
        }
        let ids = canonical_ids(&current);
        assert_eq!(ids.len(), original.len());
        assert!(indexer.reconcile_index_with_canonical_documents(
            Vec::new(), temp.path(), TierKind::Fast, "same-coarse-fingerprint", &ids,
        ).is_err(), "an unchanged count cannot vouch for identity variant {variant}");
        assert_eq!(fs::read(&path)?, before);
    }
    Ok(())
}

#[test]
fn canonical_empty_delta_still_removes_deleted_documents() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let original = rows(&indexer, 1)?;
    drop(indexer.build_and_save_index(original.clone(), temp.path())?);
    let path = vector_index_path(temp.path(), indexer.embedder_id());
    let ids = canonical_ids(&original[..1]);
    let result = indexer.reconcile_index_with_canonical_documents(
        Vec::new(), temp.path(), TierKind::Fast, "deleted-with-no-embedding", &ids,
    )?;
    assert_eq!(vector_bits(&result)?.keys().cloned().collect::<HashSet<_>>(), ids);
    drop(result);
    assert_eq!(VectorIndex::open_read_only(&path)?.record_count(), 1);
    Ok(())
}

#[test]
fn canonical_supplied_same_identity_vector_is_not_discarded_as_a_noop() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let original = rows(&indexer, 1)?;
    drop(indexer.build_and_save_index(original.clone(), temp.path())?);
    let mut replacement = original[1].clone();
    for component in &mut replacement.embedding { *component = -*component; }
    let mut expected = original.clone();
    expected[1] = replacement.clone();
    let oracle_root = tempfile::tempdir()?;
    let oracle = indexer.build_and_save_index(expected, oracle_root.path())?;
    let result = indexer.reconcile_index_with_canonical_documents(
        vec![replacement], temp.path(), TierKind::Fast, "same-identity-new-vector", &canonical_ids(&original),
    )?;
    assert_eq!(vector_bits(&result)?, vector_bits(&oracle)?);
    Ok(())
}

#[test]
fn canonical_unchanged_reconciliation_admits_empty_and_f32_generations_without_reencoding() -> Result<()> {
    for empty in [false, true] {
        let temp = tempfile::tempdir()?;
        let indexer = SemanticIndexer::new("hash", None)?;
        let path = vector_index_path(temp.path(), indexer.embedder_id());
        fs::create_dir_all(path.parent().unwrap())?;
        let mut original = if empty { Vec::new() } else { rows(&indexer, 1)? };
        if !empty { original[0].embedding[0] = 0.123_456_79; }
        let mut writer = VectorIndex::create_with_revision(
            &path, "fnv1a-384",
            coding_agent_search::indexer::semantic::HASH_VECTOR_SPACE_REVISION,
            384, Quantization::F32,
        )?.with_generation(255);
        for row in &original { writer.write_record(&doc_id(row), &row.embedding)?; }
        writer.finish()?;
        let owner = same_file::Handle::from_path(&path)?;
        let before = fs::read(&path)?;
        let result = indexer.reconcile_index_with_canonical_documents(
            Vec::new(), temp.path(), TierKind::Fast, "same-f32", &canonical_ids(&original),
        )?;
        assert_eq!(result.quantization(), Quantization::F32);
        assert_eq!(result.record_count(), original.len());
        assert_eq!(same_file::Handle::from_path(&path)?, owner);
        assert_eq!(VectorIndex::peek_compaction_gen(&path)?, 255);
        assert_eq!(fs::read(&path)?, before);
    }
    Ok(())
}

#[test]
fn canonical_unchanged_admission_rejects_unusable_persisted_vectors() -> Result<()> {
    for value in [1e-20f32, 70_000.0f32] {
        let temp = tempfile::tempdir()?;
        let indexer = SemanticIndexer::new("hash", None)?;
        let path = vector_index_path(temp.path(), indexer.embedder_id());
        fs::create_dir_all(path.parent().unwrap())?;
        let mut original = rows(&indexer, 1)?;
        original[1].embedding.fill(value);
        // Native serialization creates the negative: the input is finite,
        // but its persisted F16 representation cannot supply useful signal.
        write_generation(&path, &original, 8)?;
        let before = fs::read(&path)?;
        assert!((0..original.len()).all(|row| original[row].embedding.iter().all(|x| x.is_finite())));
        let error = indexer.reconcile_index_with_canonical_documents(
            Vec::new(), temp.path(), TierKind::Fast, "bad-stored-signal", &canonical_ids(&original),
        ).unwrap_err();
        assert!(error.to_string().contains("unusable stored vector"), "{error:#}");
        assert_eq!(fs::read(&path)?, before);
    }
    Ok(())
}

#[test]
fn canonical_unchanged_admission_requires_unique_rows_matching_the_current_space() -> Result<()> {
    for foreign in [false, true] {
        let temp = tempfile::tempdir()?;
        let indexer = SemanticIndexer::new("hash", None)?;
        let original = rows(&indexer, 1)?;
        let path = vector_index_path(temp.path(), indexer.embedder_id());
        fs::create_dir_all(path.parent().unwrap())?;
        let revision = if foreign { "foreign-revision" } else {
            coding_agent_search::indexer::semantic::HASH_VECTOR_SPACE_REVISION
        };
        let mut writer = VectorIndex::create_with_revision(&path, "fnv1a-384", revision, 384, Quantization::F16)?;
        let duplicate_rows = vec![original[0].clone(), original[0].clone(), original[2].clone()];
        let physical_rows = if foreign { &original } else { &duplicate_rows };
        for row in physical_rows { writer.write_record(&doc_id(row), &row.embedding)?; }
        writer.finish()?;
        let before = fs::read(&path)?;
        assert!(indexer.reconcile_index_with_canonical_documents(
            Vec::new(), temp.path(), TierKind::Fast, "same-row-count", &canonical_ids(&original),
        ).is_err());
        assert_eq!(fs::read(&path)?, before);
    }
    Ok(())
}

#[test]
fn canonical_unchanged_admission_refuses_live_writers_and_absent_authority() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let original = rows(&indexer, 1)?;
    drop(indexer.build_and_save_index(original.clone(), temp.path())?);
    let path = vector_index_path(temp.path(), indexer.embedder_id());
    let before = fs::read(&path)?;
    let writer = VectorIndex::open_writer(&path)?;
    assert!(indexer.reconcile_index_with_canonical_documents(
        Vec::new(), temp.path(), TierKind::Fast, "current", &canonical_ids(&original),
    ).is_err());
    assert_eq!(fs::read(&path)?, before);
    drop(writer);
    assert!(indexer.reconcile_index_with_canonical_documents(
        Vec::new(), temp.path(), TierKind::Fast, "  ", &canonical_ids(&original),
    ).is_err());
    assert_eq!(fs::read(&path)?, before);
    Ok(())
}
