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
