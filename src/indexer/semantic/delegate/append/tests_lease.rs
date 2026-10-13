//! All files belong to fresh temporary fixtures. Exercise the public vector
//! writers and the existing cancellation seam, not a second locking algorithm.

use super::*;
use crate::indexer::semantic::artifacts::BackfillArtifacts;
use std::cell::Cell;
use std::collections::BTreeMap;
use std::fs::OpenOptions;

fn rows(indexer: &SemanticIndexer, first: u64, count: usize) -> Result<Vec<EmbeddedMessage>> {
    let inputs = (0..count)
        .map(|offset| {
            EmbeddingInput::new(
                first + u64::try_from(offset).unwrap(),
                format!("append lease compiler record {offset}"),
            )
        })
        .collect::<Vec<_>>();
    indexer.embed_messages(&inputs)
}

fn fixture() -> Result<(tempfile::TempDir, SemanticIndexer, PathBuf)> {
    let directory = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    drop(indexer.build_and_save_index(rows(&indexer, 1, 16)?, directory.path())?);
    let path = vector_index_path(directory.path(), indexer.embedder_id());
    Ok((directory, indexer, path))
}

fn image(path: &Path) -> Result<(Vec<u8>, Option<Vec<u8>>)> {
    let wal = match fs::read(wal_path_for(path)) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    Ok((fs::read(path)?, wal))
}

fn view(path: &Path) -> Result<BTreeMap<String, Vec<u32>>> {
    let reader = VectorIndex::open_read_only(path)?;
    let mut result = BTreeMap::new();
    for row in 0..reader.record_count() {
        if !reader.is_deleted(row) {
            result.insert(
                reader.doc_id_at(row)?.to_owned(),
                reader.vector_at_f32(row)?.into_iter().map(f32::to_bits).collect(),
            );
        }
    }
    for (id, vector) in reader.wal_records() {
        result.insert(id.to_owned(), vector.iter().map(|value| value.to_bits()).collect());
    }
    Ok(result)
}

fn assert_busy<T>(result: Result<T>) {
    let error = result.err().expect("the artifact owner must exclude this writer");
    assert!(format!("{error:#}").contains("owns"), "wrong refusal: {error:#}");
}

fn assert_released(data: &Path) -> Result<()> {
    drop(BackfillArtifacts::for_existing_writer(data)?);
    Ok(())
}

#[test]
fn append_refuses_a_live_lease_before_consuming_input_and_retry_keeps_old_vectors() -> Result<()> {
    let (directory, indexer, path) = fixture()?;
    let data = directory.path();
    let before = image(&path)?;
    let old_vectors = view(&path)?;
    let input = rows(&indexer, 100, 1)?;
    let lease = BackfillArtifacts::for_existing_writer(data)?;
    let consumed = Cell::new(false);
    assert_busy(indexer.append_to_index(
        input.clone().into_iter().inspect(|_| consumed.set(true)), data,
    ));
    assert!(!consumed.get());
    assert_eq!(image(&path)?, before);
    drop(lease);
    assert_eq!(indexer.append_to_index(input, data)?, 1);
    let after = view(&path)?;
    assert_eq!(after.len(), old_vectors.len() + 1);
    for (id, vector) in old_vectors {
        assert_eq!(after.get(&id), Some(&vector));
    }
    assert_released(data)
}

#[test]
fn append_lease_spans_iterator_exhaustion_and_the_native_read_to_write_handoff() -> Result<()> {
    let (directory, indexer, path) = fixture()?;
    let data = directory.path();
    let before = image(&path)?;
    let old_vectors = view(&path)?;
    let current_ids = old_vectors.keys().cloned().collect::<HashSet<_>>();
    let mut input = rows(&indexer, 100, 1)?.into_iter();
    let exhausted = Cell::new(false);
    let after_exhaustion = Cell::new(0usize);
    let iterator = std::iter::from_fn(|| {
        let next = input.next();
        if next.is_none() {
            exhausted.set(true);
        }
        next
    });
    assert_eq!(run_with_limit(&indexer, iterator, data, DEFAULT_MAX_BYTES, || {
        if exhausted.get() {
            after_exhaustion.set(after_exhaustion.get() + 1);
            // The final call occurs after source and filesystem observations
            // are dropped. The native source lock cannot cover that gap.
            assert_busy(BackfillArtifacts::for_existing_writer(data));
            assert_busy(indexer.append_to_index(Vec::<EmbeddedMessage>::new(), data));
            assert_busy(indexer.build_and_save_index(Vec::<EmbeddedMessage>::new(), data));
            assert_busy(indexer.reconcile_index_with_canonical_documents(
                Vec::new(), data, TierKind::Quality, "content-v1:lease", &current_ids,
            ));
            assert_eq!(image(&path)?, before);
        }
        Ok(())
    })?, 1);
    // Exhaustion callback, prepared-batch fence, and pre-persistence fence.
    assert!(after_exhaustion.get() >= 3);
    let after = view(&path)?;
    assert_eq!(after.len(), old_vectors.len() + 1);
    for (id, vector) in old_vectors {
        assert_eq!(after.get(&id), Some(&vector));
    }
    assert_released(data)
}

#[test]
fn append_never_uses_its_lease_to_clean_unrelated_scratch_or_read_the_ledger() -> Result<()> {
    let (directory, indexer, path) = fixture()?;
    let data = directory.path();
    let scratch = data.join("vector_index/.backfill-reuse-Unrelated123");
    fs::create_dir(&scratch)?;
    let evidence = scratch.join("candidate.fsvi");
    fs::write(&evidence, b"unrelated interrupted work")?;
    let ledger = crate::search::semantic_manifest::SemanticManifest::path(data);
    fs::write(&ledger, b"{unreadable metadata is not append authority")?;
    let ledger_bytes = fs::read(&ledger)?;
    let before = image(&path)?;
    assert_eq!(indexer.append_to_index(Vec::<EmbeddedMessage>::new(), data)?, 0);
    let mut invalid = rows(&indexer, 100, 1)?;
    invalid[0].embedding[0] = f32::NAN;
    assert!(indexer.append_to_index(invalid, data).is_err());
    assert_eq!(image(&path)?, before);
    assert_eq!(indexer.append_to_index(rows(&indexer, 100, 1)?, data)?, 1);
    assert_eq!(fs::read(&evidence)?, b"unrelated interrupted work");
    assert_eq!(fs::read(&ledger)?, ledger_bytes);
    assert_released(data)
}

#[test]
fn cancellation_error_and_iterator_unwind_release_the_lease_without_persistence() -> Result<()> {
    for mode in 0..3 {
        let (directory, indexer, path) = fixture()?;
        let data = directory.path();
        let before = image(&path)?;
        let entered = Cell::new(false);
        let input = rows(&indexer, 100, 1)?.into_iter().inspect(|_| {
            entered.set(true);
            assert_busy(BackfillArtifacts::for_existing_writer(data));
            assert!(mode != 2, "intentional temporary-fixture iterator unwind");
        });
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_with_limit(&indexer, input, data, if mode == 1 { 1 } else { DEFAULT_MAX_BYTES }, || {
                if mode == 0 && entered.get() {
                    anyhow::bail!("injected cancellation before append persistence");
                }
                Ok(())
            })
        }));
        assert!(entered.get());
        if mode == 2 {
            assert!(outcome.is_err());
        } else {
            assert!(outcome.expect("only the explicit unwind case may panic").is_err());
        }
        assert_eq!(image(&path)?, before);
        assert_released(data)?;
        assert_eq!(indexer.append_to_index(rows(&indexer, 100, 1)?, data)?, 1);
    }
    Ok(())
}

#[test]
fn append_does_not_create_missing_directories_or_lock_through_a_symlinked_root() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let missing = directory.path().join("missing");
    let empty = directory.path().join("empty");
    fs::create_dir(&empty)?;
    for data in [&missing, &empty] {
        assert!(indexer.append_to_index(Vec::<EmbeddedMessage>::new(), data).is_err());
        assert!(!data.join("vector_index").exists());
        assert!(!data.join("semantic-backfill-artifacts.lock").exists());
    }
    assert!(!missing.exists());
    #[cfg(unix)]
    {
        let target = directory.path().join("target");
        fs::create_dir(&target)?;
        std::os::unix::fs::symlink(&target, empty.join("vector_index"))?;
        assert!(indexer.append_to_index(Vec::<EmbeddedMessage>::new(), &empty).is_err());
        assert_eq!(fs::read_dir(&target)?.count(), 0);
        assert!(!empty.join("semantic-backfill-artifacts.lock").exists());
    }
    Ok(())
}

#[test]
fn both_replacement_writers_exclude_append_while_their_candidate_is_private() -> Result<()> {
    for reconcile in [false, true] {
        let (directory, indexer, path) = fixture()?;
        let data = directory.path();
        let before = image(&path)?;
        let ids = view(&path)?.keys().cloned().collect::<HashSet<_>>();
        let replacements = rows(&indexer, 1, 16)?;
        let input = rows(&indexer, 100, 1)?;
        let observed = Cell::new(false);
        let check = || {
            let private = fs::read_dir(data.join("vector_index"))?
                .collect::<std::io::Result<Vec<_>>>()?
                .iter().any(|entry| entry.file_name().to_string_lossy().starts_with(".backfill-reuse-"));
            if private {
                observed.set(true);
                assert_busy(indexer.append_to_index(input.clone(), data));
                assert_eq!(image(&path)?, before);
            }
            Ok(())
        };
        let published = if reconcile {
            super::super::reconcile::run(
                &indexer, replacements, data, TierKind::Quality, "content-v1:lease", &ids, check,
            )?
        } else {
            super::super::rebuild::run(&indexer, replacements, data, None::<fn(usize)>, check)?
        };
        assert!(observed.get());
        drop(published);
        assert_released(data)?;
        assert_eq!(indexer.append_to_index(input, data)?, 1);
        // The stable lock inode survives ordinary operations; never delete it.
        let lock = OpenOptions::new().read(true).write(true)
            .open(data.join("semantic-backfill-artifacts.lock"))?;
        fs2::FileExt::try_lock_exclusive(&lock)?;
        fs2::FileExt::unlock(&lock)?;
    }
    Ok(())
}
