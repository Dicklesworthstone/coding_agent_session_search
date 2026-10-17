use super::*;
use std::collections::BTreeMap;
use std::io::ErrorKind;

fn interrupted() -> anyhow::Error {
    std::io::Error::new(ErrorKind::Interrupted, "cancelled canonical fixture").into()
}

fn assert_interrupted(error: &anyhow::Error) {
    assert_eq!(
        error.downcast_ref::<std::io::Error>().map(|e| e.kind()),
        Some(ErrorKind::Interrupted),
        "{error:#}"
    );
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

fn bytes(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn vectors(index: &VectorIndex) -> Result<BTreeMap<String, Vec<u32>>> {
    assert_eq!(index.wal_record_count(), 0);
    let mut values = BTreeMap::new();
    for row in 0..index.record_count() {
        assert!(!index.is_deleted(row));
        assert!(
            values
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
    Ok(values)
}

struct Fixture {
    root: tempfile::TempDir,
    path: PathBuf,
    before: Option<Vec<u8>>,
    wal_before: Option<Vec<u8>>,
    replacements: Vec<EmbeddedMessage>,
    current: HashSet<String>,
    expected: Vec<EmbeddedMessage>,
}

impl Fixture {
    // 0: first build, 1: full replacement, 2: delta, 3: WAL delta,
    // 4: unchanged (including opaque recovery protection), 5: delete all.
    fn new(indexer: &SemanticIndexer, scenario: usize) -> Result<Self> {
        let root = tempfile::tempdir()?;
        let path = vector_index_path(root.path(), indexer.embedder_id());
        let original = indexer.embed_messages(&[
            EmbeddingInput::new(1, "compiler original message"),
            EmbeddingInput::new(2, "network retained message"),
            EmbeddingInput::new(3, "deleted third message"),
        ])?;
        let new = indexer.embed_messages(&[
            EmbeddingInput::new(1, "edited compiler replacement"),
            EmbeddingInput::new(90, "new checkpoint message"),
        ])?;
        if scenario != 0 {
            drop(indexer.build_and_save_index(original.clone(), root.path())?);
        }
        let (replacements, expected) = match scenario {
            0 | 1 => (new.clone(), new),
            2 => (
                new.clone(),
                vec![new[0].clone(), original[1].clone(), new[1].clone()],
            ),
            3 => {
                let mut writer = VectorIndex::open_writer(&path)?;
                let mut pending = original[1].clone();
                for value in &mut pending.embedding {
                    *value = -*value;
                }
                writer.append_batch(&[(id(&pending), pending.embedding.clone())])?;
                assert_eq!(writer.wal_record_count(), 1);
                drop(writer);
                (new.clone(), vec![new[0].clone(), pending, new[1].clone()])
            }
            4 => {
                fs::write(
                    path.with_extension("fsvi.fec"),
                    b"untouched recovery fixture",
                )?;
                (Vec::new(), original)
            }
            5 => (Vec::new(), Vec::new()),
            _ => unreachable!(),
        };
        Ok(Self {
            before: bytes(&path)?,
            wal_before: bytes(&wal_path_for(&path))?,
            current: expected.iter().map(id).collect(),
            root,
            path,
            replacements,
            expected,
        })
    }

    fn assert_preserved(&self) -> Result<()> {
        assert_eq!(bytes(&self.path)?, self.before);
        assert_eq!(bytes(&wal_path_for(&self.path))?, self.wal_before);
        if let Some(parent) = self.path.parent().filter(|parent| parent.exists()) {
            for entry in fs::read_dir(parent)? {
                assert!(
                    !entry?
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".semantic-reconcile-")
                );
            }
        }
        Ok(())
    }

    fn run(
        &self,
        indexer: &SemanticIndexer,
        check: impl FnMut() -> Result<()>,
    ) -> Result<VectorIndex> {
        run(
            indexer,
            self.replacements.clone(),
            self.root.path(),
            TierKind::Fast,
            "canonical-cancellation-fixture",
            &self.current,
            check,
        )
    }
}

#[test]
fn every_canonical_cancellation_boundary_preserves_main_wal_and_readers() -> Result<()> {
    let indexer = SemanticIndexer::new("hash", None)?;
    for scenario in 0..6 {
        let control = Fixture::new(&indexer, scenario)?;
        let mut probes = 0usize;
        let output = control.run(&indexer, || {
            probes += 1;
            Ok(())
        })?;
        let oracle_root = tempfile::tempdir()?;
        let oracle_path = oracle_root.path().join("reference.fsvi");
        let mut oracle = VectorIndex::create_with_revision(
            &oracle_path,
            indexer.embedder_id(),
            expected_vector_space_revision(indexer.embedder_id()).unwrap(),
            indexer.embedder_dimension(),
            Quantization::F16,
        )?;
        for row in &control.expected {
            oracle.write_record(&id(row), &row.embedding)?;
        }
        oracle.finish()?;
        assert_eq!(
            vectors(&output)?,
            vectors(&VectorIndex::open_read_only(&oracle_path)?)?
        );
        assert!(probes >= 4);
        for fail_at in 1..=probes {
            let fixture = Fixture::new(&indexer, scenario)?;
            let retained = if fixture.before.is_some() {
                Some(VectorIndex::open_read_only(&fixture.path)?)
            } else {
                None
            };
            let mut observed = 0usize;
            let error = fixture
                .run(&indexer, || {
                    observed += 1;
                    if observed == fail_at {
                        Err(interrupted())
                    } else {
                        Ok(())
                    }
                })
                .unwrap_err();
            assert_interrupted(&error);
            assert_eq!(observed, fail_at);
            fixture.assert_preserved()?;
            if let Some(retained) = retained {
                let fresh = VectorIndex::open_read_only(&fixture.path)?;
                assert_eq!(fresh.record_count(), retained.record_count());
                assert_eq!(fresh.wal_record_count(), retained.wal_record_count());
            }
            if scenario == 4 {
                assert_eq!(
                    fs::read(fixture.path.with_extension("fsvi.fec"))?,
                    b"untouched recovery fixture"
                );
            }
        }
    }
    Ok(())
}

#[test]
fn cancellation_after_native_finish_does_not_install_complete_candidate() -> Result<()> {
    let indexer = SemanticIndexer::new("hash", None)?;
    for scenario in [0, 1, 2, 3, 5] {
        let fixture = Fixture::new(&indexer, scenario)?;
        let mut observed_candidate = false;
        let error = fixture
            .run(&indexer, || {
                let parent = fixture.path.parent().unwrap();
                if parent.exists() {
                    for entry in fs::read_dir(parent)? {
                        let entry = entry?;
                        if entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with(".semantic-reconcile-")
                            && entry.path().join("candidate.fsvi").exists()
                        {
                            observed_candidate = true;
                            return Err(interrupted());
                        }
                    }
                }
                Ok(())
            })
            .unwrap_err();
        assert_interrupted(&error);
        assert!(observed_candidate, "must cancel after real writer finish");
        fixture.assert_preserved()?;
    }
    Ok(())
}

#[test]
fn canonical_success_has_no_post_publication_cancellation_verdict() -> Result<()> {
    let indexer = SemanticIndexer::new("hash", None)?;
    for scenario in [0, 1, 2, 3, 5] {
        let fixture = Fixture::new(&indexer, scenario)?;
        let result = fixture.run(&indexer, || {
            if bytes(&fixture.path)? != fixture.before {
                Err(interrupted())
            } else {
                Ok(())
            }
        })?;
        assert_eq!(result.record_count(), fixture.current.len());
        assert_ne!(bytes(&fixture.path)?, fixture.before);
    }
    Ok(())
}

#[test]
fn replacement_handoff_borrows_canonical_keys_and_releases_original_vectors() -> Result<()> {
    let indexer = SemanticIndexer::new("hash", None)?;
    let fixture = Fixture::new(&indexer, 0)?;
    let mut replacements = prepare_replacements(
        &indexer,
        fixture.replacements.clone(),
        &fixture.current,
        &mut || Ok(()),
    )?;
    for (&key, vector) in &replacements {
        assert!(vector.is_some());
        assert!(std::ptr::eq(
            key.as_ptr(),
            fixture.current.get(key).unwrap().as_ptr()
        ));
    }
    let output = fixture.root.path().join("handoff.fsvi");
    let mut writer = VectorIndex::create_with_revision(
        &output,
        indexer.embedder_id(),
        HASH_VECTOR_SPACE_REVISION,
        indexer.embedder_dimension(),
        Quantization::F16,
    )?;
    let mut remaining = fixture.current.iter().map(String::as_str).collect();
    write_replacements(&mut replacements, &mut writer, &mut remaining, &mut || {
        Ok(())
    })?;
    assert!(remaining.is_empty());
    assert!(replacements.values().all(Option::is_none));
    assert_eq!(replacements.len(), fixture.current.len());
    assert!(!output.exists(), "writer is still uncommitted");
    writer.finish()?;
    let actual = VectorIndex::open_read_only(&output)?;
    assert_eq!(actual.record_count(), fixture.current.len());
    for row in 0..actual.record_count() {
        assert!(fixture.current.contains(actual.doc_id_at(row)?));
        assert!(actual.is_vector_usable(row));
    }
    Ok(())
}

#[test]
fn cancelled_handoff_never_finishes_a_partial_replacement() -> Result<()> {
    let indexer = SemanticIndexer::new("hash", None)?;
    let fixture = Fixture::new(&indexer, 0)?;
    let mut replacements = prepare_replacements(
        &indexer,
        fixture.replacements.clone(),
        &fixture.current,
        &mut || Ok(()),
    )?;
    let output = fixture.root.path().join("partial.fsvi");
    let mut writer = VectorIndex::create_with_revision(
        &output,
        indexer.embedder_id(),
        HASH_VECTOR_SPACE_REVISION,
        indexer.embedder_dimension(),
        Quantization::F16,
    )?;
    let mut remaining = fixture.current.iter().map(String::as_str).collect();
    let mut probes = 0;
    let error = write_replacements(&mut replacements, &mut writer, &mut remaining, &mut || {
        probes += 1;
        if probes == 2 {
            Err(interrupted())
        } else {
            Ok(())
        }
    })
    .unwrap_err();
    assert_interrupted(&error);
    assert_eq!(
        replacements
            .values()
            .filter(|value| value.is_none())
            .count(),
        1
    );
    assert_eq!(remaining.len(), fixture.current.len() - 1);
    drop(writer);
    assert!(!output.exists());
    fixture.assert_preserved()?;
    Ok(())
}
