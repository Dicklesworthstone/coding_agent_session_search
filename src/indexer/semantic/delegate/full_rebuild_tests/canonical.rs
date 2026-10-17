//! Exercise the public canonical path against real hash embeddings and FSVI.
//! No synthetic storage, overridden dependency, or private engine shortcut.
use super::*;
use std::collections::BTreeMap;

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

fn ids(rows: &[EmbeddedMessage]) -> HashSet<String> {
    rows.iter().map(id).collect()
}

fn bit_map(index: &VectorIndex) -> Result<BTreeMap<String, Vec<u32>>> {
    assert_eq!(index.wal_record_count(), 0);
    let mut rows = BTreeMap::new();
    for position in 0..index.record_count() {
        if !index.is_deleted(position) {
            assert!(
                rows.insert(
                    index.doc_id_at(position)?.to_owned(),
                    index
                        .vector_at_f32(position)?
                        .into_iter()
                        .map(f32::to_bits)
                        .collect(),
                )
                .is_none(),
                "result must not contain ambiguous duplicate identities"
            );
        }
    }
    Ok(rows)
}

fn write_source(
    path: &Path,
    rows: &[EmbeddedMessage],
    quantization: Quantization,
    generation: u8,
    revision: &str,
) -> Result<()> {
    fs::create_dir_all(path.parent().context("fixture needs a parent")?)?;
    let mut writer =
        VectorIndex::create_with_revision(path, "fnv1a-384", revision, 384, quantization)?
            .with_generation(generation);
    for row in rows {
        writer.write_record(&id(row), &row.embedding)?;
    }
    writer.finish()?;
    Ok(())
}

fn extra(indexer: &SemanticIndexer, message_id: u64) -> Result<EmbeddedMessage> {
    indexer
        .embed_messages(&[EmbeddingInput::new(
            message_id,
            format!("new canonical compiler result number {message_id}"),
        )])?
        .pop()
        .context("nonempty hash input must produce a vector")
}

fn reconcile(
    indexer: &SemanticIndexer,
    root: &Path,
    replacements: Vec<EmbeddedMessage>,
    current: &[EmbeddedMessage],
) -> Result<VectorIndex> {
    indexer.reconcile_index_with_canonical_documents(
        replacements,
        root,
        TierKind::Fast,
        "exact-canonical-test-snapshot",
        &ids(current),
    )
}

#[test]
fn initial_canonical_publication_requires_complete_valid_coverage() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("not-created-yet");
    let indexer = SemanticIndexer::new("hash", None)?;
    let rows = embedded(&indexer)?;
    assert!(reconcile(&indexer, &root, rows[..1].to_vec(), &rows).is_err());
    assert!(
        !root.exists(),
        "missing coverage must fail before scratch I/O"
    );
    let mut invalid = rows.clone();
    invalid[1].embedding[0] = f32::NAN;
    assert!(reconcile(&indexer, &root, invalid, &rows).is_err());
    assert!(!root.exists(), "invalid input must fail before scratch I/O");
    assert!(
        indexer
            .reconcile_index_with_canonical_documents(
                rows.clone(),
                &root,
                TierKind::Fast,
                "",
                &ids(&rows),
            )
            .is_err()
    );
    assert!(!root.exists());

    let actual = reconcile(&indexer, &root, rows.clone(), &rows)?;
    let oracle_root = tempfile::tempdir()?;
    let oracle = indexer.build_and_save_index(rows, oracle_root.path())?;
    assert_eq!(bit_map(&actual)?, bit_map(&oracle)?);
    let path = vector_index_path(&root, indexer.embedder_id());
    assert_eq!(actual.path(), path);
    assert_eq!(VectorIndex::peek_compaction_gen(&path)?, 1);
    drop(actual);
    assert_eq!(
        bit_map(&VectorIndex::open_read_only(&path)?)?,
        bit_map(&oracle)?
    );
    Ok(())
}

#[test]
fn empty_initial_canonical_generation_is_complete_and_then_retained() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let first = reconcile(&indexer, temp.path(), Vec::new(), &[])?;
    assert_eq!(first.record_count(), 0);
    let path = first.path().to_path_buf();
    drop(first);
    let before = ObservedSemanticFile::capture(&path)?;
    let retained = reconcile(&indexer, temp.path(), Vec::new(), &[])?;
    assert_eq!(retained.record_count(), 0);
    assert_eq!(ObservedSemanticFile::capture(&path)?, before);
    // A proved no-op returns a shared owner, not a long-lived exclusive writer.
    let another = VectorIndex::open_read_only(&path)?;
    assert_eq!(another.record_count(), 0);
    Ok(())
}

#[test]
fn no_wal_full_replacement_advances_generation_and_rejects_a_delayed_old_log() -> Result<()> {
    for generation in [1, 17, 254, 255] {
        let temp = tempfile::tempdir()?;
        let indexer = SemanticIndexer::new("hash", None)?;
        let original = embedded(&indexer)?;
        let path = vector_index_path(temp.path(), indexer.embedder_id());
        write_source(
            &path,
            &original,
            Quantization::F16,
            generation,
            HASH_VECTOR_SPACE_REVISION,
        )?;
        // Serialize a genuine old-generation WAL on an independent sibling.
        // It is deliberately absent at the destination until AFTER publication.
        let producer = temp.path().join("old-log-producer.fsvi");
        fs::copy(&path, &producer)?;
        let mut log_writer = VectorIndex::open_writer(&producer)?;
        let pending = extra(&indexer, 800)?;
        log_writer.append_batch(&[(id(&pending), pending.embedding)])?;
        drop(log_writer);
        let delayed_log = fs::read(wal_path_for(&producer))?;
        assert!(!wal_path_for(&path).exists());
        let retained = VectorIndex::open_read_only(&path)?;
        let original_bits = bit_map(&retained)?;
        let replacements = vec![extra(&indexer, 40)?, extra(&indexer, 50)?];
        let oracle_root = tempfile::tempdir()?;
        let oracle = indexer.build_and_save_index(replacements.clone(), oracle_root.path())?;
        let actual = reconcile(&indexer, temp.path(), replacements.clone(), &replacements)?;
        assert_eq!(
            VectorIndex::peek_compaction_gen(&path)?,
            next_generation(generation)
        );
        assert_eq!(bit_map(&actual)?, bit_map(&oracle)?);
        assert_eq!(bit_map(&retained)?, original_bits);
        drop(actual);
        fs::write(wal_path_for(&path), &delayed_log)?;
        let reopened = VectorIndex::open_read_only(&path)?;
        assert_eq!(
            reopened.wal_record_count(),
            0,
            "old WAL cannot resurrect deleted rows"
        );
        assert_eq!(bit_map(&reopened)?, bit_map(&oracle)?);
        assert_eq!(
            fs::read(wal_path_for(&path))?,
            delayed_log,
            "reader must not clean fixtures"
        );
    }
    Ok(())
}

#[test]
fn no_wal_delta_preserves_f32_bits_and_old_readers_while_retiring_tombstones() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let mut original = embedded(&indexer)?;
    original[0].embedding[0] = 0.123_456_79;
    let path = vector_index_path(temp.path(), indexer.embedder_id());
    write_source(
        &path,
        &original,
        Quantization::F32,
        41,
        HASH_VECTOR_SPACE_REVISION,
    )?;
    let mut writer = VectorIndex::open_writer(&path)?;
    assert!(writer.soft_delete(&id(&original[2]))?);
    drop(writer);
    assert!(!wal_path_for(&path).exists());
    let retained = VectorIndex::open_read_only(&path)?;
    let old_bits = bit_map(&retained)?;
    let edited = extra(&indexer, original[1].message_id)?;
    let added = extra(&indexer, 90)?;
    let expected = vec![original[0].clone(), edited.clone(), added.clone()];
    let actual = reconcile(&indexer, temp.path(), vec![edited, added], &expected)?;
    assert_eq!(actual.quantization(), Quantization::F32);
    assert_eq!(actual.tombstone_count(), 0);
    assert_eq!(VectorIndex::peek_compaction_gen(&path)?, 42);
    let expected_bits = expected
        .iter()
        .map(|row| {
            (
                id(row),
                row.embedding
                    .iter()
                    .map(|v| v.to_bits())
                    .collect::<Vec<_>>(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(bit_map(&actual)?, expected_bits);
    assert_eq!(retained.tombstone_count(), 1);
    assert_eq!(bit_map(&retained)?, old_bits);
    Ok(())
}

#[test]
fn no_wal_partial_reuse_cannot_invent_missing_or_tombstoned_vectors() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let original = embedded(&indexer)?;
    let path = vector_index_path(temp.path(), indexer.embedder_id());
    write_source(
        &path,
        &original,
        Quantization::F16,
        19,
        HASH_VECTOR_SPACE_REVISION,
    )?;
    let mut writer = VectorIndex::open_writer(&path)?;
    writer.soft_delete(&id(&original[2]))?;
    drop(writer);
    let before = fs::read(&path)?;
    for missing in [original[2].clone(), extra(&indexer, 400)?] {
        let expected = vec![original[0].clone(), missing];
        let error = reconcile(&indexer, temp.path(), vec![original[0].clone()], &expected)
            .expect_err("unavailable retained vectors must require canonical re-embedding");
        assert!(error.to_string().contains("lacks vectors"), "{error:#}");
        assert_eq!(fs::read(&path)?, before);
        assert!(!wal_path_for(&path).exists());
    }
    Ok(())
}

#[test]
fn no_wal_complete_replacement_respects_live_writer_ownership() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let original = embedded(&indexer)?;
    let path = vector_index_path(temp.path(), indexer.embedder_id());
    write_source(
        &path,
        &original,
        Quantization::F16,
        7,
        HASH_VECTOR_SPACE_REVISION,
    )?;
    let writer = VectorIndex::open_writer(&path)?;
    let before = fs::read(&path)?;
    let replacements = vec![extra(&indexer, 40)?];
    let error = reconcile(&indexer, temp.path(), replacements.clone(), &replacements)
        .expect_err("a copied snapshot is not permission to replace a writer-owned artifact");
    assert!(matches!(error.downcast_ref::<frankensearch::SearchError>(),
        Some(frankensearch::SearchError::InvalidConfig { field, .. }) if field == "fsvi.map_lock"));
    assert_eq!(fs::read(&path)?, before);
    drop(writer);
    let actual = reconcile(&indexer, temp.path(), replacements.clone(), &replacements)?;
    assert_eq!(actual.record_count(), 1);
    assert_eq!(VectorIndex::peek_compaction_gen(&path)?, 8);
    Ok(())
}

#[test]
fn no_wal_replacement_rejects_bad_input_and_f16_signal_loss_before_install() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let original = embedded(&indexer)?;
    let path = vector_index_path(temp.path(), indexer.embedder_id());
    write_source(
        &path,
        &original,
        Quantization::F16,
        19,
        HASH_VECTOR_SPACE_REVISION,
    )?;
    let before = fs::read(&path)?;
    for failure in 0..7 {
        let mut replacement = original.clone();
        match failure {
            0 => {
                replacement[1].embedding.pop();
            }
            1 => replacement[1].embedding[0] = f32::NAN,
            2 => replacement[1].embedding[0] = f32::INFINITY,
            3 => replacement[1].embedding.fill(0.0),
            4 => replacement[1].embedding.fill(1e-20),
            5 => replacement[1].embedding.fill(70_000.0),
            _ => replacement.push(original[0].clone()),
        }
        let error = reconcile(&indexer, temp.path(), replacement, &original)
            .expect_err("rejected input or stored representation must not replace the source");
        if failure == 4 || failure == 5 {
            assert!(
                error.to_string().contains("unusable persisted"),
                "{error:#}"
            );
        }
        assert_eq!(fs::read(&path)?, before, "failure {failure}");
        assert!(!wal_path_for(&path).exists());
    }
    Ok(())
}

#[test]
fn no_wal_duplicate_source_requires_replacement_of_the_ambiguous_identity() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let original = embedded(&indexer)?;
    let path = vector_index_path(temp.path(), indexer.embedder_id());
    let mut duplicate = original[0].clone();
    for value in &mut duplicate.embedding {
        *value = -*value;
    }
    write_source(
        &path,
        &[original[0].clone(), duplicate, original[1].clone()],
        Quantization::F32,
        11,
        HASH_VECTOR_SPACE_REVISION,
    )?;
    let before = fs::read(&path)?;
    let current = &original[..2];
    let error = reconcile(&indexer, temp.path(), vec![original[1].clone()], current)
        .expect_err("duplicate retained identities cannot be resolved by arbitrary row order");
    assert!(
        error.to_string().contains("duplicate current document"),
        "{error:#}"
    );
    assert_eq!(fs::read(&path)?, before);
    let actual = reconcile(&indexer, temp.path(), vec![original[0].clone()], current)?;
    let expected = current
        .iter()
        .map(|row| {
            (
                id(row),
                row.embedding
                    .iter()
                    .map(|v| v.to_bits())
                    .collect::<Vec<_>>(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(bit_map(&actual)?, expected);
    Ok(())
}

#[test]
fn no_wal_foreign_space_requires_complete_replacement_and_a_successor() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let original = embedded(&indexer)?;
    let path = vector_index_path(temp.path(), indexer.embedder_id());
    write_source(
        &path,
        &original,
        Quantization::F32,
        73,
        "foreign-input-contract",
    )?;
    let before = fs::read(&path)?;
    let error = reconcile(&indexer, temp.path(), original[..1].to_vec(), &original)
        .expect_err("same dimensions do not authorize partial foreign-space reuse");
    assert!(
        error.to_string().contains("incompatible vector space"),
        "{error:#}"
    );
    assert_eq!(fs::read(&path)?, before);
    let actual = reconcile(&indexer, temp.path(), original.clone(), &original)?;
    assert_eq!(actual.embedder_revision(), HASH_VECTOR_SPACE_REVISION);
    assert_eq!(VectorIndex::peek_compaction_gen(&path)?, 74);
    Ok(())
}

#[test]
fn initial_canonical_publication_preserves_an_orphaned_acknowledged_wal() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let original = embedded(&indexer)?;
    let producer = temp.path().join("wal-owner.fsvi");
    write_source(
        &producer,
        &original,
        Quantization::F16,
        1,
        HASH_VECTOR_SPACE_REVISION,
    )?;
    let mut writer = VectorIndex::open_writer(&producer)?;
    let pending = extra(&indexer, 40)?;
    writer.append_batch(&[(id(&pending), pending.embedding)])?;
    drop(writer);
    let root = temp.path().join("orphan");
    let path = vector_index_path(&root, indexer.embedder_id());
    fs::create_dir_all(path.parent().context("fixture parent")?)?;
    let wal = fs::read(wal_path_for(&producer))?;
    fs::write(wal_path_for(&path), &wal)?;
    let error = reconcile(&indexer, &root, original.clone(), &original)
        .expect_err("a complete input set does not authorize destroying an orphan WAL");
    assert!(error.to_string().contains("orphan WAL"), "{error:#}");
    assert!(!path.exists());
    assert_eq!(fs::read(wal_path_for(&path))?, wal);
    Ok(())
}

#[test]
fn no_wal_differential_deltas_match_independent_quantized_builds() -> Result<()> {
    let indexer = SemanticIndexer::new("hash", None)?;
    let inputs = (1..=32)
        .map(|message_id| {
            EmbeddingInput::new(
                message_id,
                format!("canonical session {message_id} compiler checkpoint"),
            )
        })
        .collect::<Vec<_>>();
    let original = indexer.embed_messages(&inputs)?;
    for quantization in [Quantization::F16, Quantization::F32] {
        for round in 0u64..24 {
            let temp = tempfile::tempdir()?;
            let path = vector_index_path(temp.path(), indexer.embedder_id());
            write_source(
                &path,
                &original,
                quantization,
                37,
                HASH_VECTOR_SPACE_REVISION,
            )?;
            let retained = VectorIndex::open_read_only(&path)?;
            let before = bit_map(&retained)?;
            let mut replacements = Vec::new();
            let mut current = Vec::new();
            for row in &original {
                match (row.message_id.wrapping_mul(17).wrapping_add(round)) % 5 {
                    0 => {} // canonical deletion
                    1 => {
                        let mut replacement = extra(&indexer, row.message_id)?;
                        replacement.source_id = 7;
                        replacements.push(replacement.clone());
                        current.push(replacement);
                    }
                    _ => {
                        let mut kept = row.clone();
                        // The oracle uses exactly the stored old vector, not
                        // a freshly embedded approximation of retained rows.
                        let stored = before.get(&id(row)).context("fixture identity")?;
                        kept.embedding = stored.iter().copied().map(f32::from_bits).collect();
                        current.push(kept);
                    }
                }
            }
            let added = extra(&indexer, 1000 + round)?;
            replacements.push(added.clone());
            current.push(added);
            let oracle_path = temp.path().join("independent.fsvi");
            write_source(
                &oracle_path,
                &current,
                quantization,
                38,
                HASH_VECTOR_SPACE_REVISION,
            )?;
            let oracle = VectorIndex::open_read_only(&oracle_path)?;
            let actual = reconcile(&indexer, temp.path(), replacements, &current)?;
            assert_eq!(actual.quantization(), quantization);
            assert_eq!(actual.tombstone_count(), 0);
            assert_eq!(
                bit_map(&actual)?,
                bit_map(&oracle)?,
                "{quantization:?}, round {round}"
            );
            assert_eq!(bit_map(&retained)?, before);
            assert_eq!(VectorIndex::peek_compaction_gen(&path)?, 38);
        }
    }
    Ok(())
}
