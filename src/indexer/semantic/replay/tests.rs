//! Exercise the public facade and bounded replay against real FrankenStorage.
use super::*;
use crate::franken_sync::FrankenError;
use crate::model::types::{Agent, AgentKind, MessageRole};
use std::cell::Cell;
use std::path::Path;

fn database(root: &Path, parents: usize, messages_per_parent: usize) -> Result<FrankenStorage> {
    let storage = FrankenStorage::open(&root.join("archive.db"))?;
    let agent = storage.ensure_agent(&Agent {
        id: None,
        slug: "codex".into(),
        name: "Codex".into(),
        version: None,
        kind: AgentKind::Cli,
    })?;
    let workspace = storage.ensure_workspace(Path::new("/canonical-replay/workspace"), None)?;
    for parent in 0..parents {
        storage.insert_conversation_tree(
            agent,
            Some(workspace),
            &Conversation {
                id: None,
                agent_slug: "codex".into(),
                workspace: Some(PathBuf::from("/canonical-replay/workspace")),
                external_id: Some(format!("replay-{parent}")),
                title: Some(format!("replay {parent}")),
                source_path: format!("/canonical-replay/{parent}.jsonl").into(),
                started_at: Some(1),
                ended_at: Some(2),
                approx_tokens: None,
                metadata_json: serde_json::Value::Null,
                source_id: "remote-laptop".into(),
                origin_host: Some("FixtureHost".into()),
                messages: (0..messages_per_parent)
                    .map(|ordinal| Message {
                        id: None,
                        idx: ordinal as i64,
                        role: MessageRole::User,
                        author: Some("fixture".into()),
                        created_at: Some(1_700_000_000_000 + ordinal as i64),
                        content: format!("canonical compiler evidence {parent} {ordinal}"),
                        extra_json: serde_json::Value::Null,
                        snippets: Vec::new(),
                    })
                    .collect(),
            },
        )?;
    }
    Ok(storage)
}

fn signature(inputs: Vec<engine::EmbeddingInput>) -> Vec<(Option<String>, String)> {
    inputs
        .into_iter()
        .map(|input| (engine::semantic_doc_id_for_input(&input), input.content))
        .collect()
}

#[derive(Debug, thiserror::Error)]
#[error("replay consumer stopped")]
struct Stop;

#[test]
fn facade_replay_matches_incumbent_across_parent_and_message_pages() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = database(temp.path(), PARENT_PAGE_SIZE + 1, 3)?;
    storage.raw().execute_compat(
        "UPDATE messages SET content = ?1, role = 'tool' WHERE id = 1",
        &[ParamValue::from(
            "Unicode cafe\u{301} 東京 deployment evidence ".repeat(400),
        )],
    )?;
    storage
        .raw()
        .execute("UPDATE messages SET content = '', role = 'system' WHERE id = 2")?;
    let expected = engine::packet_embedding_inputs_from_storage(&storage)?;
    let mut actual = Vec::new();
    crate::indexer::semantic::visit_packet_embedding_inputs_from_storage(&storage, |input| {
        actual.push(input);
        Ok(())
    })?;
    assert!(actual.len() > (PARENT_PAGE_SIZE + 1) * 3);
    assert_eq!(signature(actual), signature(expected));
    storage.close()?;
    Ok(())
}

#[test]
fn large_history_consumer_runs_before_an_unadmitted_corrupt_tail_is_decoded() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = database(temp.path(), 1, pages::MAX_ROWS + 1)?;
    storage.raw().execute(&format!(
        "UPDATE messages SET role = X'00' WHERE idx = {}",
        pages::MAX_ROWS,
    ))?;
    // The actual old path collects the full conversation before invoking any
    // visitor. The new path admits only one bounded message page at a time.
    let mut old_delivered = 0;
    let old_error = engine::visit_packet_embedding_inputs_from_storage(&storage, |_| {
        old_delivered += 1;
        Err(Stop.into())
    })
    .unwrap_err();
    assert_eq!(old_delivered, 0);
    assert!(!old_error.is::<Stop>());
    let mut delivered = 0;
    let error = visit(
        &storage,
        |_| {
            delivered += 1;
            Err(Stop.into())
        },
        || Ok(()),
    )
    .unwrap_err();
    assert!(error.is::<Stop>());
    assert_eq!(delivered, 1);
    let mut delivered = 0;
    let error = visit(
        &storage,
        |_| {
            delivered += 1;
            Ok(())
        },
        || Ok(()),
    )
    .unwrap_err();
    assert!(error.downcast_ref::<FrankenError>().is_some());
    assert_eq!(delivered, pages::MAX_ROWS);
    storage.close()?;
    Ok(())
}

#[test]
fn consumer_queries_the_same_archive_after_every_message_page() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = database(temp.path(), 1, pages::MAX_ROWS * 2 + 1)?;
    let expected = engine::packet_embedding_inputs_from_storage(&storage)?;
    storage.raw().execute("BEGIN")?;
    let mut actual = Vec::new();
    visit(
        &storage,
        |input| {
            let count: i64 = storage.raw().query_row_map(
                "SELECT COUNT(*) FROM messages",
                &[] as &[ParamValue],
                |row| row.get_typed(0),
            )?;
            assert_eq!(count, (pages::MAX_ROWS * 2 + 1) as i64);
            assert!(storage.raw().as_async().in_transaction());
            actual.push(input);
            Ok(())
        },
        || Ok(()),
    )?;
    assert!(storage.raw().as_async().in_transaction());
    storage.raw().execute("ROLLBACK")?;
    assert_eq!(signature(actual), signature(expected));
    storage.close()?;
    Ok(())
}

#[test]
fn cancellation_and_consumer_errors_cannot_certify_partial_replay() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = database(temp.path(), 2, 3)?;
    let mut delivered = 0;
    let error = visit(
        &storage,
        |_| {
            delivered += 1;
            Ok(())
        },
        || Err(Stop.into()),
    )
    .unwrap_err();
    assert!(error.is::<Stop>());
    assert_eq!(delivered, 0);
    let cancelled = Cell::new(false);
    let mut delivered = 0;
    let error = visit(
        &storage,
        |_| {
            delivered += 1;
            cancelled.set(true);
            Ok(())
        },
        || {
            if cancelled.get() {
                Err(Stop.into())
            } else {
                Ok(())
            }
        },
    )
    .unwrap_err();
    assert!(error.is::<Stop>());
    assert_eq!(delivered, 1);
    let mut replayed = 0;
    visit(
        &storage,
        |_| {
            replayed += 1;
            Ok(())
        },
        || Ok(()),
    )?;
    assert_eq!(
        replayed, 6,
        "a failed visit must neither consume durable progress nor poison storage"
    );
    storage.close()?;
    Ok(())
}

#[test]
fn stale_empty_parent_pages_and_signed_parent_ids_are_not_skipped() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = database(temp.path(), PARENT_PAGE_SIZE + 1, 1)?;
    storage.raw().execute("DELETE FROM messages")?;
    let agent: i64 = storage.raw().query_row_map(
        "SELECT id FROM agents WHERE slug = 'codex'",
        &[] as &[ParamValue],
        |row| row.get_typed(0),
    )?;
    for (ordinal, id) in [i64::MIN, -1, 0, i64::MAX].into_iter().enumerate() {
        storage.raw().execute_compat(
            "INSERT INTO conversations(id, agent_id, source_id, external_id, source_path)
             VALUES (?1, ?2, 'local', ?3, ?4)",
            &[
                ParamValue::from(id),
                ParamValue::from(agent),
                ParamValue::from(format!("signed-{ordinal}")),
                ParamValue::from(format!("/signed/{ordinal}.jsonl")),
            ],
        )?;
        storage.raw().execute_compat(
            "INSERT INTO messages(conversation_id, idx, role, content, created_at) VALUES (?1, 0, 'user', ?2, 1)",
            &[ParamValue::from(id), ParamValue::from(format!("signed evidence {ordinal}"))],
        )?;
    }
    let mut contents = Vec::new();
    visit(
        &storage,
        |input| {
            contents.push(input.content);
            Ok(())
        },
        || Ok(()),
    )?;
    assert_eq!(
        contents,
        (0..4)
            .map(|i| format!("signed evidence {i}"))
            .collect::<Vec<_>>()
    );
    storage.close()?;
    Ok(())
}

#[test]
fn single_parent_replay_stays_scoped_and_stops_before_later_body_pages() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = database(temp.path(), 2, pages::MAX_ROWS + 1)?;
    let parents: Vec<i64> = storage.raw().query_map_collect(
        "SELECT id FROM conversations ORDER BY id",
        &[] as &[ParamValue],
        |row| row.get_typed(0),
    )?;
    let target = parents[1];
    storage.raw().execute_compat(
        "UPDATE messages SET role = X'00' WHERE conversation_id = ?1 AND idx = ?2",
        &[
            ParamValue::from(target),
            ParamValue::from(pages::MAX_ROWS as i64),
        ],
    )?;

    let mut delivered = 0usize;
    let error = visit_packet_embedding_inputs_for_conversation(
        &storage,
        target,
        |_| {
            delivered += 1;
            Err(Stop.into())
        },
        || Ok(()),
    )
    .expect_err("consumer stop must preempt the corrupt later page");
    assert!(error.is::<Stop>());
    assert_eq!(delivered, 1);

    let foreign: i64 = storage.raw().query_row_map(
        "SELECT COUNT(*) FROM messages WHERE conversation_id = ?1",
        &[ParamValue::from(parents[0])],
        |row| row.get_typed(0),
    )?;
    assert_eq!(foreign, (pages::MAX_ROWS + 1) as i64);
    storage.close()?;
    Ok(())
}

#[test]
fn direct_semantic_progress_precedes_an_unadmitted_corrupt_history_tail() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = database(temp.path(), 1, pages::MAX_ROWS + 1)?;
    storage.raw().execute(&format!(
        "UPDATE messages SET role = X'00' WHERE idx = {}",
        pages::MAX_ROWS,
    ))?;
    let mut progress = Vec::new();
    let error = crate::indexer::semantic::packet_embedding_inputs_from_storage_with_progress(
        &storage,
        |current, total| progress.push((current, total)),
    )
    .unwrap_err();
    assert!(error.downcast_ref::<FrankenError>().is_some());
    assert!(
        progress
            .iter()
            .filter(|&&(done, total)| done == 0 && total == 1)
            .count()
            >= 2,
        "planning and real bounded message work must report before decoding the tail: {progress:?}"
    );
    assert!(
        !progress.contains(&(1, 1)),
        "failed replay cannot report completion"
    );
    let count: i64 = storage.raw().query_row_map(
        "SELECT COUNT(*) FROM messages",
        &[] as &[ParamValue],
        |row| row.get_typed(0),
    )?;
    assert_eq!(count, (pages::MAX_ROWS + 1) as i64);
    storage.close()?;
    Ok(())
}

#[test]
fn direct_semantic_progress_replay_includes_signed_canonical_parents() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = database(temp.path(), 1, 1)?;
    let agent: i64 = storage.raw().query_row_map(
        "SELECT id FROM agents WHERE slug = 'codex'",
        &[] as &[ParamValue],
        |row| row.get_typed(0),
    )?;
    for id in [-1_i64, 0] {
        storage.raw().execute_compat(
            "INSERT INTO conversations(id, agent_id, source_id, external_id, source_path) VALUES (?1, ?2, 'local', ?3, ?4)",
            &[ParamValue::from(id), ParamValue::from(agent), ParamValue::from(format!("direct-signed-{id}")), ParamValue::from(format!("/direct-signed/{id}.jsonl"))],
        )?;
        storage.raw().execute_compat(
            "INSERT INTO messages(conversation_id, idx, role, content, created_at) VALUES (?1, 0, 'user', ?2, 1)",
            &[ParamValue::from(id), ParamValue::from(format!("signed direct semantic evidence {id}"))],
        )?;
    }
    let mut expected = Vec::new();
    visit(
        &storage,
        |input| {
            expected.push(input);
            Ok(())
        },
        || Ok(()),
    )?;
    let mut progress = Vec::new();
    let actual = crate::indexer::semantic::packet_embedding_inputs_from_storage_with_progress(
        &storage,
        |current, total| progress.push((current, total)),
    )?;
    assert_eq!(
        actual.len(),
        3,
        "a zero-initialized cursor must not skip legacy parents"
    );
    assert_eq!(signature(actual), signature(expected));
    assert_eq!(progress.last(), Some(&(3, 3)));
    storage.close()?;
    Ok(())
}

#[test]
fn direct_semantic_progress_matches_legacy_content_and_actual_nonempty_parent_count() -> Result<()>
{
    let temp = tempfile::tempdir()?;
    let storage = database(temp.path(), PARENT_PAGE_SIZE + 2, 3)?;
    storage.raw().execute_compat(
        "UPDATE messages SET content = ?1, role = 'tool' WHERE id = 1",
        &[ParamValue::from(
            "Unicode café 東京, complete tool evidence. ".repeat(1200),
        )],
    )?;
    storage
        .raw()
        .execute("UPDATE messages SET content = '' WHERE conversation_id = 2")?;
    let expected = engine::packet_embedding_inputs_from_storage_with_progress(&storage, |_, _| {})?;
    let mut progress = Vec::new();
    let actual = crate::indexer::semantic::packet_embedding_inputs_from_storage_with_progress(
        &storage,
        |current, total| progress.push((current, total)),
    )?;
    assert_eq!(signature(actual), signature(expected));
    assert_eq!(
        progress.last(),
        Some(&(PARENT_PAGE_SIZE + 2, PARENT_PAGE_SIZE + 2))
    );
    assert!(
        progress
            .windows(2)
            .all(|w| w[0].0 <= w[1].0 && w[0].1 == w[1].1)
    );
    storage
        .raw()
        .execute("DELETE FROM messages WHERE conversation_id = 3")?;
    let mut progress = Vec::new();
    crate::indexer::semantic::packet_embedding_inputs_from_storage_with_progress(
        &storage,
        |current, total| progress.push((current, total)),
    )?;
    assert_eq!(
        progress.last(),
        Some(&(PARENT_PAGE_SIZE + 1, PARENT_PAGE_SIZE + 1)),
        "stale nonempty tail hints must not inflate progress; blank parents still count"
    );
    assert!(!storage.raw().as_async().in_transaction());
    storage.close()?;
    Ok(())
}

#[test]
fn direct_semantic_progress_preserves_caller_transactions_on_success_and_unwind() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = database(temp.path(), 1, pages::MAX_ROWS + 1)?;
    storage.raw().execute("BEGIN")?;
    let expected = crate::indexer::semantic::packet_embedding_inputs_from_storage_with_progress(
        &storage,
        |_, _| {},
    )?;
    assert_eq!(expected.len(), pages::MAX_ROWS + 1);
    assert!(storage.raw().as_async().in_transaction());
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = crate::indexer::semantic::packet_embedding_inputs_from_storage_with_progress(
            &storage,
            |_, _| panic!("interrupt direct replay progress"),
        );
    }));
    assert!(outcome.is_err());
    assert!(
        storage.raw().as_async().in_transaction(),
        "borrowed transaction must remain caller-owned"
    );
    storage.raw().execute("ROLLBACK")?;
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = crate::indexer::semantic::packet_embedding_inputs_from_storage_with_progress(
            &storage,
            |_, _| panic!("interrupt owned replay snapshot"),
        );
    }));
    assert!(outcome.is_err());
    assert!(
        !storage.raw().as_async().in_transaction(),
        "owned snapshot must release on unwind"
    );
    assert_eq!(
        crate::indexer::semantic::packet_embedding_inputs_from_storage_with_progress(
            &storage,
            |_, _| {}
        )?
        .len(),
        expected.len()
    );
    storage.close()?;
    Ok(())
}

#[test]
fn direct_semantic_progress_snapshot_does_not_mix_concurrent_canonical_rewrites() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = database(temp.path(), 2, 1)?;
    let writer = FrankenStorage::open(&temp.path().join("archive.db"))?;
    let mut before = Vec::new();
    visit(
        &storage,
        |input| {
            before.push(input);
            Ok(())
        },
        || Ok(()),
    )?;
    let mut changed = false;
    let actual = crate::indexer::semantic::packet_embedding_inputs_from_storage_with_progress(
        &storage,
        |_, _| {
            if !changed {
                writer.raw().execute("UPDATE messages SET content = 'later committed semantic snapshot evidence' WHERE id = 2").unwrap();
                changed = true;
            }
        },
    )?;
    assert!(changed);
    assert_eq!(signature(actual), signature(before));
    assert!(!storage.raw().as_async().in_transaction());
    let later = crate::indexer::semantic::packet_embedding_inputs_from_storage_with_progress(
        &storage,
        |_, _| {},
    )?;
    assert!(
        later
            .iter()
            .any(|input| input.content == "later committed semantic snapshot evidence")
    );
    writer.close()?;
    storage.close()?;
    Ok(())
}

#[test]
fn direct_semantic_progress_empty_archive_reports_no_fabricated_work() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = database(temp.path(), PARENT_PAGE_SIZE + 1, 0)?;
    let mut progress = Vec::new();
    let inputs = crate::indexer::semantic::packet_embedding_inputs_from_storage_with_progress(
        &storage,
        |current, total| progress.push((current, total)),
    )?;
    assert!(inputs.is_empty());
    assert_eq!(progress, vec![(0, 0)]);
    assert!(!storage.raw().as_async().in_transaction());
    storage.close()?;
    Ok(())
}

fn stored_semantic_rows(
    root: &Path,
    indexer: &crate::indexer::semantic::SemanticIndexer,
) -> Result<std::collections::BTreeMap<String, Vec<f32>>> {
    let path = crate::search::vector_index::vector_index_path(root, indexer.embedder_id());
    let index = frankensearch::index::VectorIndex::open_read_only(&path)?;
    (0..index.record_count())
        .map(|row| Ok((index.doc_id_at(row)?.to_owned(), index.vector_at_f32(row)?)))
        .collect()
}

#[test]
fn direct_semantic_streaming_build_matches_incumbent_vectors_across_batches() -> Result<()> {
    use crate::indexer::semantic::{SemanticIndexer, SemanticStorageBuildPhase};

    let temp = tempfile::tempdir()?;
    let storage = database(temp.path(), 1, 515)?;
    storage.raw().execute_compat(
        "UPDATE messages SET content = ?1, role = 'tool' WHERE id = 1",
        &[ParamValue::from(
            "complete Unicode café 東京 compiler and transaction evidence. ".repeat(20_000),
        )],
    )?;
    storage
        .raw()
        .execute("UPDATE messages SET content = '' WHERE id = 515")?;
    storage.set_last_embedded_message_id(7)?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let mut inputs =
        engine::packet_embedding_inputs_from_storage_with_progress(&storage, |_, _| {})?;
    inputs.retain(|input| {
        !crate::search::canonicalize::is_hard_message_noise(
            crate::indexer::semantic_role_name(input.role),
            &input.content,
        )
    });
    let expected_count = inputs.len();
    let incumbent = temp.path().join("incumbent");
    drop(indexer.build_and_save_index(indexer.embed_messages(&inputs)?, &incumbent)?);
    drop(inputs);
    let streaming = temp.path().join("streaming");
    let mut embed_batches = 0;
    let outcome = indexer.build_and_save_index_from_storage_with_progress(
        &storage,
        &streaming,
        |phase, current, total| {
            if phase == SemanticStorageBuildPhase::Embedding && current == 0 {
                assert!(total <= 512, "source text must stay batch-bounded");
                embed_batches += 1;
            }
        },
    )?;
    assert!(embed_batches > 1);
    assert_eq!(outcome.embedded_docs, expected_count);
    assert_eq!(outcome.last_message_id, Some(515));
    assert_eq!(
        stored_semantic_rows(&streaming, &indexer)?,
        stored_semantic_rows(&incumbent, &indexer)?
    );
    assert_eq!(storage.get_last_embedded_message_id()?, Some(7));
    assert!(!storage.raw().as_async().in_transaction());
    storage.close()?;
    Ok(())
}

#[test]
fn direct_semantic_streaming_late_replay_error_preserves_published_generation() -> Result<()> {
    use crate::indexer::semantic::{EmbeddingInput, SemanticIndexer, SemanticStorageBuildPhase};

    let temp = tempfile::tempdir()?;
    let storage = database(temp.path(), 1, 577)?;
    storage.set_last_embedded_message_id(11)?;
    let indexer = SemanticIndexer::new("hash", None)?;
    drop(indexer.build_and_save_index(
        indexer.embed_messages(&[EmbeddingInput::new(
            999,
            "prior committed compiler evidence",
        )])?,
        temp.path(),
    )?);
    let path = crate::search::vector_index::vector_index_path(temp.path(), indexer.embedder_id());
    let before = std::fs::read(&path)?;
    // The corrupt page follows enough valid rows to produce a real embedding
    // batch. A fallible replay cannot turn that earlier prefix into a publish.
    storage
        .raw()
        .execute("UPDATE messages SET role = X'00' WHERE idx = 512")?;
    let mut embedded_prefix = false;
    let error = indexer
        .build_and_save_index_from_storage_with_progress(
            &storage,
            temp.path(),
            |phase, current, total| {
                if phase == SemanticStorageBuildPhase::Embedding && current > 0 && current == total
                {
                    embedded_prefix = true;
                }
            },
        )
        .unwrap_err();
    assert!(
        embedded_prefix,
        "the failure must follow useful embedded work"
    );
    assert!(error.downcast_ref::<FrankenError>().is_some());
    assert_eq!(std::fs::read(&path)?, before);
    assert_eq!(storage.get_last_embedded_message_id()?, Some(11));
    assert!(!storage.raw().as_async().in_transaction());
    storage.close()?;
    Ok(())
}

#[test]
fn direct_semantic_streaming_build_and_watermark_share_one_snapshot() -> Result<()> {
    use crate::indexer::semantic::{SemanticIndexer, SemanticStorageBuildPhase};

    let temp = tempfile::tempdir()?;
    let storage = database(temp.path(), 2, 3)?;
    let writer = FrankenStorage::open(&temp.path().join("archive.db"))?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let before = temp.path().join("before");
    let inputs = engine::packet_embedding_inputs_from_storage(&storage)?;
    drop(indexer.build_and_save_index(indexer.embed_messages(&inputs)?, &before)?);
    let mut changed = false;
    let streaming = temp.path().join("streaming");
    let outcome = indexer.build_and_save_index_from_storage_with_progress(
        &storage,
        &streaming,
        |phase, _, _| {
            if phase == SemanticStorageBuildPhase::Replay && !changed {
                writer.raw().execute(
                    "INSERT INTO messages(conversation_id, idx, role, content, created_at) VALUES (2, 99, 'user', 'later concurrent archive evidence', 1700000000100)",
                ).expect("concurrent canonical append");
                changed = true;
            }
        },
    )?;
    assert!(changed);
    assert_eq!(outcome.embedded_docs, 6);
    assert_eq!(outcome.last_message_id, Some(6));
    assert_eq!(storage.max_message_id()?, Some(7));
    assert_eq!(
        stored_semantic_rows(&streaming, &indexer)?,
        stored_semantic_rows(&before, &indexer)?
    );
    assert!(!storage.raw().as_async().in_transaction());
    writer.close()?;
    storage.close()?;
    Ok(())
}

#[test]
fn direct_semantic_streaming_all_noise_replaces_old_vectors_and_reports_cursor() -> Result<()> {
    use crate::indexer::semantic::{EmbeddingInput, SemanticIndexer};

    let temp = tempfile::tempdir()?;
    let storage = database(temp.path(), 1, 2)?;
    storage.raw().execute("UPDATE messages SET content = ''")?;
    let indexer = SemanticIndexer::new("hash", None)?;
    drop(indexer.build_and_save_index(
        indexer.embed_messages(&[EmbeddingInput::new(
            99,
            "retired semantic compiler evidence",
        )])?,
        temp.path(),
    )?);
    let outcome = indexer.build_and_save_index_from_storage_with_progress(
        &storage,
        temp.path(),
        |_, _, _| {},
    )?;
    assert_eq!(outcome.embedded_docs, 0);
    assert_eq!(outcome.last_message_id, Some(2));
    assert!(stored_semantic_rows(temp.path(), &indexer)?.is_empty());
    storage.close()?;
    Ok(())
}

#[test]
#[cfg(target_os = "linux")]
#[ignore = "subprocess measurement worker; invoked explicitly by paired memory test"]
fn direct_semantic_memory_worker() -> Result<()> {
    let root = std::env::var_os("CASS_DIRECT_REPLAY_MEMORY_ROOT").expect("measurement archive");
    let mode = std::env::var("CASS_DIRECT_REPLAY_MEMORY_MODE").expect("measurement mode");
    let storage = FrankenStorage::open_readonly(&PathBuf::from(root).join("archive.db"))?;
    let started = std::time::Instant::now();
    let inputs = match mode.as_str() {
        "legacy" => {
            engine::packet_embedding_inputs_from_storage_with_progress(&storage, |_, _| {})?
        }
        "bounded" => crate::indexer::semantic::packet_embedding_inputs_from_storage_with_progress(
            &storage,
            |_, _| {},
        )?,
        _ => anyhow::bail!("invalid measurement mode"),
    };
    let elapsed = started.elapsed().as_micros();
    let mut digest = blake3::Hasher::new();
    for input in &inputs {
        let identity = engine::semantic_doc_id_for_input(input).context("measurement identity")?;
        digest.update(&(identity.len() as u64).to_le_bytes());
        digest.update(identity.as_bytes());
        digest.update(&(input.content.len() as u64).to_le_bytes());
        digest.update(input.content.as_bytes());
    }
    let status = std::fs::read_to_string("/proc/self/status")?;
    let rss = status
        .lines()
        .find(|line| line.starts_with("VmHWM:"))
        .context("Linux peak RSS")?
        .split_whitespace()
        .nth(1)
        .context("peak RSS number")?;
    println!(
        "DIRECT_REPLAY_MEMORY mode={mode} inputs={} digest={} elapsed_us={elapsed} peak_rss_kib={rss}",
        inputs.len(),
        digest.finalize().to_hex()
    );
    storage.close()?;
    Ok(())
}

#[test]
#[cfg(target_os = "linux")]
#[ignore = "paired synthetic history replay and RSS measurement; execute explicitly"]
fn direct_semantic_paired_memory_measurement() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let messages = 4096;
    let storage = database(temp.path(), 1, messages)?;
    let text =
        "canonical tool transcript evidence with complete context and Unicode 東京. ".repeat(220);
    storage.raw().execute_compat(
        "UPDATE messages SET content = ?1",
        &[ParamValue::from(text)],
    )?;
    storage.close()?;
    let mut reference = None;
    for mode in ["legacy", "bounded", "bounded", "legacy"] {
        let output = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--ignored",
                "--exact",
                "indexer::semantic::replay::tests::direct_semantic_memory_worker",
                "--nocapture",
            ])
            .env("CASS_DIRECT_REPLAY_MEMORY_ROOT", temp.path())
            .env("CASS_DIRECT_REPLAY_MEMORY_MODE", mode)
            .env("CASS_SEMANTIC_STREAM_REPLAY", "1")
            .output()?;
        ensure!(
            output.status.success(),
            "measurement child failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout)?;
        let line = stdout
            .lines()
            .find(|line| line.starts_with("DIRECT_REPLAY_MEMORY "))
            .context("measurement receipt")?;
        let identity: Vec<_> = line
            .split_whitespace()
            .filter(|field| field.starts_with("inputs=") || field.starts_with("digest="))
            .map(str::to_owned)
            .collect();
        if let Some(expected) = &reference {
            assert_eq!(
                &identity, expected,
                "both modes must retain every canonical passage"
            );
        } else {
            reference = Some(identity);
        }
        eprintln!("{line}");
    }
    Ok(())
}
