use super::*;
use std::cell::Cell;
use std::collections::BTreeMap;

fn rows(indexer: &SemanticIndexer, start: u64, count: usize) -> Result<Vec<EmbeddedMessage>> {
    let inputs = (0..count)
        .map(|offset| {
            EmbeddingInput::new(
                start + u64::try_from(offset).unwrap(),
                format!("compiler cache entry {offset}"),
            )
        })
        .collect::<Vec<_>>();
    indexer.embed_messages(&inputs)
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

fn write_rows(path: &Path, rows: &[EmbeddedMessage], quantization: Quantization) -> Result<()> {
    fs::create_dir_all(path.parent().unwrap())?;
    let mut writer = VectorIndex::create_with_revision(
        path,
        "fnv1a-384",
        HASH_VECTOR_SPACE_REVISION,
        384,
        quantization,
    )?;
    for row in rows {
        writer.write_record(&id(row), &row.embedding)?;
    }
    writer.finish()?;
    Ok(())
}

fn bytes(path: &Path) -> Result<(Vec<u8>, Option<Vec<u8>>)> {
    let wal = match fs::read(wal_path_for(path)) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    Ok((fs::read(path)?, wal))
}

fn cold_view(path: &Path) -> Result<BTreeMap<String, Vec<u32>>> {
    let index = VectorIndex::open_read_only(path)?;
    let mut values = BTreeMap::new();
    for row in 0..index.record_count() {
        if !index.is_deleted(row) {
            values.insert(
                index.doc_id_at(row)?.to_owned(),
                index
                    .vector_at_f32(row)?
                    .into_iter()
                    .map(f32::to_bits)
                    .collect(),
            );
        }
    }
    for (id, vector) in index.wal_records() {
        values.insert(
            id.to_owned(),
            vector.iter().map(|value| value.to_bits()).collect(),
        );
    }
    Ok(values)
}

fn seeded_source(
    quantization: Quantization,
) -> Result<(tempfile::TempDir, SemanticIndexer, PathBuf)> {
    let root = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let path = vector_index_path(root.path(), indexer.embedder_id());
    write_rows(&path, &rows(&indexer, 1, 16)?, quantization)?;
    let pending = rows(&indexer, 100, 1)?;
    let mut writer = VectorIndex::open_writer(&path)?;
    writer.append_batch(&[(id(&pending[0]), pending[0].embedding.clone())])?;
    drop(writer);
    Ok((root, indexer, path))
}

#[test]
fn prepared_last_occurrence_order_matches_independent_reverse_dedup() -> Result<()> {
    let indexer = SemanticIndexer::new("hash", None)?;
    let source = rows(&indexer, 1, 31)?;
    for seed in 0..128u64 {
        let mut state = seed + 1;
        let mut input = Vec::new();
        for position in 0..257usize {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let slot = usize::try_from(state % 31).unwrap();
            let mut row = source[slot].clone();
            row.embedding[0] = f32::from(u16::try_from(position + 1)?) / 512.0;
            input.push(row);
        }
        let mut seen = HashSet::new();
        let mut expected = Vec::new();
        for row in input.iter().rev() {
            if seen.insert(id(row)) {
                expected.push((id(row), row.embedding.clone()));
            }
        }
        expected.reverse();
        let prepared =
            Prepared::collect(input, 384, Quantization::F16, DEFAULT_MAX_BYTES, || Ok(()))?;
        assert_eq!(prepared.input_count, 257);
        let actual = prepared
            .into_ordered_messages()?
            .into_iter()
            .map(|row| (id(&row), row.embedding))
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
    }
    Ok(())
}

#[test]
fn repeated_input_retains_only_one_vector_per_identity() -> Result<()> {
    let indexer = SemanticIndexer::new("hash", None)?;
    let row = rows(&indexer, 1, 1)?.remove(0);
    let one = Prepared::collect(
        [row.clone()],
        384,
        Quantization::F16,
        DEFAULT_MAX_BYTES,
        || Ok(()),
    )?;
    let limit = one.owned_bytes;
    let prepared = Prepared::collect(
        (0..100_000).map(|_| row.clone()),
        384,
        Quantization::F16,
        limit,
        || Ok(()),
    )?;
    assert_eq!(prepared.input_count, 100_000);
    assert_eq!(prepared.entries.len(), 1);
    assert_eq!(prepared.owned_bytes, limit);
    Ok(())
}

#[test]
fn retained_capacity_not_just_vector_length_counts_against_budget() -> Result<()> {
    let indexer = SemanticIndexer::new("hash", None)?;
    let mut row = rows(&indexer, 1, 1)?.remove(0);
    row.embedding.reserve_exact(65_536);
    let error = Prepared::collect([row], 384, Quantization::F16, 4096, || Ok(())).unwrap_err();
    assert!(matches!(error.downcast_ref::<SearchError>(),
        Some(SearchError::InvalidConfig { field, .. }) if field == "semantic_append.max_bytes"));
    Ok(())
}

#[test]
fn enormous_iterator_size_hint_is_not_an_allocation_instruction() -> Result<()> {
    struct Hint(Option<EmbeddedMessage>);
    impl Iterator for Hint {
        type Item = EmbeddedMessage;
        fn next(&mut self) -> Option<Self::Item> {
            self.0.take()
        }
        fn size_hint(&self) -> (usize, Option<usize>) {
            (usize::MAX, Some(usize::MAX))
        }
    }
    let indexer = SemanticIndexer::new("hash", None)?;
    let row = rows(&indexer, 1, 1)?.remove(0);
    let prepared = Prepared::collect(Hint(Some(row)), 384, Quantization::F16, 4096, || Ok(()))?;
    assert_eq!(prepared.input_count, 1);
    Ok(())
}

#[test]
fn budget_refusal_preserves_the_entire_live_batch_and_retry_is_complete() -> Result<()> {
    let (root, indexer, path) = seeded_source(Quantization::F16)?;
    let input = rows(&indexer, 200, 3)?;
    let before = bytes(&path)?;
    let limit = Prepared::collect(
        [input[0].clone()],
        384,
        Quantization::F16,
        DEFAULT_MAX_BYTES,
        || Ok(()),
    )?
    .owned_bytes;
    let error = run_with_limit(&indexer, input.clone(), root.path(), limit, || Ok(())).unwrap_err();
    assert!(matches!(error.downcast_ref::<SearchError>(),
        Some(SearchError::InvalidConfig { field, .. }) if field == "semantic_append.max_bytes"));
    assert_eq!(bytes(&path)?, before);
    assert_eq!(
        run_with_limit(
            &indexer,
            input.clone(),
            root.path(),
            DEFAULT_MAX_BYTES,
            || Ok(())
        )?,
        3,
    );
    let actual = cold_view(&path)?;
    for row in input {
        assert!(actual.contains_key(&id(&row)));
    }
    assert_eq!(actual.len(), 20);
    Ok(())
}

#[test]
fn all_invalid_positions_preserve_main_and_acknowledged_wal() -> Result<()> {
    let (root, indexer, path) = seeded_source(Quantization::F16)?;
    let original = rows(&indexer, 200, 3)?;
    let before = bytes(&path)?;
    let hits = cold_view(&path)?;
    for position in 0..3 {
        for kind in 0..7 {
            let mut input = original.clone();
            match kind {
                0 => {
                    input[position].embedding.pop();
                }
                1 => input[position].embedding[0] = f32::NAN,
                2 => input[position].embedding[0] = f32::INFINITY,
                3 => input[position].embedding[0] = f32::NEG_INFINITY,
                4 => input[position].embedding.fill(0.0),
                5 => input[position].embedding.fill(1e-20),
                _ => input[position].embedding.fill(70_000.0),
            }
            assert!(indexer.append_to_index(input, root.path()).is_err());
            assert_eq!(bytes(&path)?, before, "kind {kind}, position {position}");
            assert_eq!(cold_view(&path)?, hits);
        }
    }
    Ok(())
}

#[test]
fn an_invalid_superseded_occurrence_still_rejects_the_whole_request() -> Result<()> {
    let (root, indexer, path) = seeded_source(Quantization::F16)?;
    let good = rows(&indexer, 200, 1)?.remove(0);
    let mut bad = good.clone();
    bad.embedding[0] = f32::NAN;
    let before = bytes(&path)?;
    assert!(indexer.append_to_index([bad, good], root.path()).is_err());
    assert_eq!(bytes(&path)?, before);
    Ok(())
}

#[test]
fn f32_storage_does_not_inherit_f16_range_or_signal_limits() -> Result<()> {
    let (root, indexer, path) = seeded_source(Quantization::F32)?;
    let mut input = rows(&indexer, 200, 2)?;
    input[0].embedding.fill(1e-20);
    input[1].embedding.fill(70_000.0);
    assert_eq!(indexer.append_to_index(input.clone(), root.path())?, 2);
    let actual = cold_view(&path)?;
    for row in input {
        assert_eq!(
            actual[&id(&row)],
            row.embedding
                .iter()
                .map(|v| v.to_bits())
                .collect::<Vec<_>>(),
        );
    }
    Ok(())
}

#[test]
fn public_append_matches_native_atomic_batch_after_restart() -> Result<()> {
    for quantization in [Quantization::F16, Quantization::F32] {
        let (root, indexer, path) = seeded_source(quantization)?;
        let oracle_root = tempfile::tempdir()?;
        let oracle_path = vector_index_path(oracle_root.path(), indexer.embedder_id());
        fs::create_dir_all(oracle_path.parent().unwrap())?;
        fs::copy(&path, &oracle_path)?;
        fs::copy(wal_path_for(&path), wal_path_for(&oracle_path))?;
        let mut input = rows(&indexer, 1, 5)?;
        let mut replacement = input[0].clone();
        for value in &mut replacement.embedding {
            *value = -*value;
        }
        input.push(replacement);
        input.extend(rows(&indexer, 500, 2)?);
        let mut different_scope = input[0].clone();
        different_scope.source_id = 2;
        input.push(different_scope);
        let mut native = VectorIndex::open_writer(&oracle_path)?;
        native.append_batch(
            &input
                .iter()
                .map(|row| (id(row), row.embedding.clone()))
                .collect::<Vec<_>>(),
        )?;
        if native.needs_compaction() {
            native.compact()?;
        }
        drop(native);
        assert_eq!(
            indexer.append_to_index(input.clone(), root.path())?,
            input.len()
        );
        assert_eq!(cold_view(&path)?, cold_view(&oracle_path)?);
    }
    Ok(())
}

#[test]
fn empty_append_never_upgrades_an_existing_shared_owner() -> Result<()> {
    let (root, indexer, path) = seeded_source(Quantization::F16)?;
    let retained = VectorIndex::open_read_only(&path)?;
    let before = bytes(&path)?;
    assert_eq!(indexer.append_to_index(Vec::new(), root.path())?, 0);
    assert_eq!(bytes(&path)?, before);
    assert_eq!(retained.wal_record_count(), 1);
    Ok(())
}

#[test]
fn input_unwind_does_not_touch_the_live_writer() -> Result<()> {
    let (root, indexer, path) = seeded_source(Quantization::F16)?;
    let before = bytes(&path)?;
    let input = rows(&indexer, 200, 3)?;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let input = input.into_iter().enumerate().map(|(position, row)| {
            assert_ne!(position, 1, "intentional iterator interruption");
            row
        });
        let _ = indexer.append_to_index(input, root.path());
    }));
    assert!(result.is_err());
    assert_eq!(bytes(&path)?, before);
    Ok(())
}

#[test]
fn cancellation_from_the_final_next_is_checked_before_commit() -> Result<()> {
    let (root, indexer, path) = seeded_source(Quantization::F16)?;
    let before = bytes(&path)?;
    let cancelled = Cell::new(false);
    let mut input = rows(&indexer, 200, 2)?.into_iter();
    let input = std::iter::from_fn(|| {
        let value = input.next();
        if value.is_none() {
            cancelled.set(true);
        }
        value
    });
    let result = run_with_limit(&indexer, input, root.path(), DEFAULT_MAX_BYTES, || {
        ensure!(!cancelled.get(), "cancelled before commit");
        Ok(())
    });
    assert!(result.unwrap_err().to_string().contains("cancelled"));
    assert_eq!(bytes(&path)?, before);
    Ok(())
}

#[test]
fn conflicting_shared_owner_prevents_commit_without_a_prefix() -> Result<()> {
    let (root, indexer, path) = seeded_source(Quantization::F16)?;
    let retained = VectorIndex::open_read_only(&path)?;
    let before = bytes(&path)?;
    assert!(
        indexer
            .append_to_index(rows(&indexer, 200, 3)?, root.path())
            .is_err()
    );
    assert_eq!(bytes(&path)?, before);
    drop(retained);
    Ok(())
}

#[test]
fn a_foreign_source_is_refused_before_consuming_the_input() -> Result<()> {
    let root = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let path = vector_index_path(root.path(), indexer.embedder_id());
    fs::create_dir_all(path.parent().unwrap())?;
    VectorIndex::create_with_revision(&path, "fnv1a-384", "foreign", 384, Quantization::F16)?
        .finish()?;
    let consumed = Cell::new(0);
    let before = bytes(&path)?;
    let input = rows(&indexer, 200, 2)?
        .into_iter()
        .inspect(|_| consumed.set(consumed.get() + 1));
    assert!(indexer.append_to_index(input, root.path()).is_err());
    assert_eq!(consumed.get(), 0);
    assert_eq!(bytes(&path)?, before);
    Ok(())
}

#[cfg(unix)]
#[test]
fn same_generation_replacement_during_iteration_is_not_appended_to() -> Result<()> {
    let root = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let path = vector_index_path(root.path(), indexer.embedder_id());
    write_rows(&path, &rows(&indexer, 1, 3)?, Quantization::F16)?;
    let replacement = root.path().join("other.fsvi");
    write_rows(&replacement, &rows(&indexer, 100, 4)?, Quantization::F16)?;
    assert_eq!(
        VectorIndex::peek_compaction_gen(&replacement)?,
        VectorIndex::peek_compaction_gen(&path)?,
    );
    let competing = fs::read(&replacement)?;
    let mut replaced = false;
    let input = rows(&indexer, 200, 2)?.into_iter().inspect(|_| {
        if !replaced {
            fs::rename(&replacement, &path).unwrap();
            replaced = true;
        }
    });
    assert!(indexer.append_to_index(input, root.path()).is_err());
    assert_eq!(fs::read(&path)?, competing);
    assert!(!wal_path_for(&path).exists());
    Ok(())
}
