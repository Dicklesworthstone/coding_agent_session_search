//! Real native writers and publication paths, with invocation-local cancellation.
//! No environment mutation, network/model download, or process-global test hook.

use super::*;
use std::cell::Cell;
use std::io::ErrorKind;

fn interrupted() -> anyhow::Error {
    std::io::Error::new(ErrorKind::Interrupted, "cancelled rebuild fixture").into()
}

fn check(flag: &Cell<bool>) -> Result<()> {
    if flag.get() {
        Err(interrupted())
    } else {
        Ok(())
    }
}

fn assert_interrupted(error: &anyhow::Error) {
    assert_eq!(
        error.downcast_ref::<std::io::Error>().map(|e| e.kind()),
        Some(ErrorKind::Interrupted),
        "{error:#}"
    );
}

fn file_bytes(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn installed_fixture(
    indexer: &SemanticIndexer,
    data_dir: &Path,
    existing: bool,
) -> Result<(PathBuf, Option<Vec<u8>>)> {
    if existing {
        drop(indexer.build_and_save_index(embedded(indexer)?, data_dir)?);
    }
    let path = vector_index_path(data_dir, indexer.embedder_id());
    let before = file_bytes(&path)?;
    Ok((path, before))
}

#[test]
fn cancelled_first_rebuild_does_not_consume_input_or_create_directories() -> Result<()> {
    let root = tempfile::tempdir()?;
    let data_dir = root.path().join("not-created");
    let indexer = SemanticIndexer::new("hash", None)?;
    let messages = std::iter::from_fn(|| -> Option<EmbeddedMessage> {
        panic!("cancelled work must not request input")
    });
    let error = rebuild::run(&indexer, messages, &data_dir, None::<fn(usize)>, || {
        Err(interrupted())
    })
    .unwrap_err();
    assert_interrupted(&error);
    assert!(!data_dir.exists());
    Ok(())
}

#[test]
fn cancellation_in_final_none_never_publishes_the_completed_prefix() -> Result<()> {
    let indexer = SemanticIndexer::new("hash", None)?;
    for existing in [false, true] {
        let root = tempfile::tempdir()?;
        let (path, before) = installed_fixture(&indexer, root.path(), existing)?;
        let flag = Cell::new(false);
        let mut source = embedded(&indexer)?.into_iter();
        let messages = std::iter::from_fn(|| {
            let message = source.next();
            if message.is_none() {
                flag.set(true);
            }
            message
        });
        let error = rebuild::run(&indexer, messages, root.path(), None::<fn(usize)>, || {
            check(&flag)
        })
        .unwrap_err();
        assert_interrupted(&error);
        assert_eq!(file_bytes(&path)?, before);
        assert!(!wal_path_for(&path).exists());
    }
    Ok(())
}

struct CancelOnDrop<'a> {
    messages: std::vec::IntoIter<EmbeddedMessage>,
    flag: &'a Cell<bool>,
}

impl Iterator for CancelOnDrop<'_> {
    type Item = EmbeddedMessage;

    fn next(&mut self) -> Option<Self::Item> {
        self.messages.next()
    }
}

impl Drop for CancelOnDrop<'_> {
    fn drop(&mut self) {
        self.flag.set(true);
    }
}

#[test]
fn cancellation_in_iterator_drop_is_checked_before_publication() -> Result<()> {
    let indexer = SemanticIndexer::new("hash", None)?;
    for existing in [false, true] {
        let root = tempfile::tempdir()?;
        let (path, before) = installed_fixture(&indexer, root.path(), existing)?;
        let flag = Cell::new(false);
        let messages = CancelOnDrop {
            messages: embedded(&indexer)?.into_iter(),
            flag: &flag,
        };
        let error = rebuild::run(&indexer, messages, root.path(), None::<fn(usize)>, || {
            check(&flag)
        })
        .unwrap_err();
        assert_interrupted(&error);
        assert_eq!(file_bytes(&path)?, before);
    }
    Ok(())
}

#[test]
fn progress_cancellation_stops_before_another_input_and_preserves_destination() -> Result<()> {
    let indexer = SemanticIndexer::new("hash", None)?;
    for existing in [false, true] {
        for cancel_at in 1..=3 {
            let root = tempfile::tempdir()?;
            let (path, before) = installed_fixture(&indexer, root.path(), existing)?;
            let flag = Cell::new(false);
            let consumed = Cell::new(0);
            let messages = embedded(&indexer)?.into_iter().inspect(|_| {
                consumed.set(consumed.get() + 1);
            });
            let mut progress = Vec::new();
            let error = rebuild::run(
                &indexer,
                messages,
                root.path(),
                Some(|accepted| {
                    progress.push(accepted);
                    if accepted == cancel_at {
                        flag.set(true);
                    }
                }),
                || check(&flag),
            )
            .unwrap_err();
            assert_interrupted(&error);
            assert_eq!(consumed.get(), cancel_at);
            assert_eq!(progress, (1..=cancel_at).collect::<Vec<_>>());
            assert_eq!(file_bytes(&path)?, before);
        }
    }
    Ok(())
}

#[test]
fn every_rebuild_cancellation_boundary_preserves_the_selected_generation() -> Result<()> {
    let indexer = SemanticIndexer::new("hash", None)?;
    for existing in [false, true] {
        let control = tempfile::tempdir()?;
        installed_fixture(&indexer, control.path(), existing)?;
        let mut probe_count = 0usize;
        drop(rebuild::run(
            &indexer,
            embedded(&indexer)?,
            control.path(),
            None::<fn(usize)>,
            || {
                probe_count += 1;
                Ok(())
            },
        )?);
        assert!(
            probe_count > 12,
            "must cover input, finish, validation, install"
        );
        for fail_at in 1..=probe_count {
            let root = tempfile::tempdir()?;
            let (path, before) = installed_fixture(&indexer, root.path(), existing)?;
            let retained = if existing {
                Some(VectorIndex::open_read_only(&path)?)
            } else {
                None
            };
            let mut seen = 0usize;
            let error = rebuild::run(
                &indexer,
                embedded(&indexer)?,
                root.path(),
                None::<fn(usize)>,
                || {
                    seen += 1;
                    if seen == fail_at {
                        Err(interrupted())
                    } else {
                        Ok(())
                    }
                },
            )
            .unwrap_err();
            assert_interrupted(&error);
            assert_eq!(seen, fail_at);
            assert_eq!(file_bytes(&path)?, before, "cancel probe {fail_at}");
            assert!(!wal_path_for(&path).exists());
            if let Some(retained) = retained {
                assert_eq!(retained.record_count(), 3);
                assert_eq!(VectorIndex::open_read_only(&path)?.record_count(), 3);
            }
        }
    }
    Ok(())
}

#[test]
fn successful_publication_is_not_reclassified_by_a_late_cancellation_probe() -> Result<()> {
    let root = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let path = vector_index_path(root.path(), indexer.embedder_id());
    let published = rebuild::run(
        &indexer,
        embedded(&indexer)?,
        root.path(),
        None::<fn(usize)>,
        || {
            if path.exists() {
                Err(interrupted())
            } else {
                Ok(())
            }
        },
    )?;
    assert!(path.exists());
    assert_eq!(published.record_count(), 3);
    Ok(())
}

#[test]
fn duplicate_full_rebuild_identities_are_rejected_without_replacing_data() -> Result<()> {
    let indexer = SemanticIndexer::new("hash", None)?;
    for existing in [false, true] {
        for different_vector in [false, true] {
            let root = tempfile::tempdir()?;
            let (path, before) = installed_fixture(&indexer, root.path(), existing)?;
            let mut rows = embedded(&indexer)?;
            let mut repeated = rows[0].clone();
            if different_vector {
                for value in &mut repeated.embedding {
                    *value = -*value;
                }
            }
            // Non-adjacent input must be caught after native physical sorting.
            rows.push(repeated);
            let error = indexer.build_and_save_index(rows, root.path()).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("duplicate semantic document identity")
            );
            assert_eq!(file_bytes(&path)?, before);
            assert!(!wal_path_for(&path).exists());
        }
    }
    Ok(())
}

#[test]
fn distinct_chunks_and_provenance_of_one_message_are_not_duplicate_identities() -> Result<()> {
    let root = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let original = embedded(&indexer)?[0].clone();
    let mut rows = vec![original.clone()];
    for field in 0..6 {
        let mut row = original.clone();
        match field {
            0 => row.chunk_idx = 1,
            1 => row.agent_id = 1,
            2 => row.workspace_id = 1,
            3 => row.source_id = 1,
            4 => row.created_at_ms = 1,
            _ => row.content_hash = [7; 32],
        }
        rows.push(row);
    }
    let published = indexer.build_and_save_index(rows, root.path())?;
    assert_eq!(published.record_count(), 7);
    let mut ids = HashSet::new();
    for row in 0..published.record_count() {
        assert!(ids.insert(published.doc_id_at(row)?));
    }
    Ok(())
}
