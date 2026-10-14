//! Real storage and the incumbent projection, not replacement storage mocks.
use super::*;
use crate::franken_sync::compat::{ConnectionExt, ParamValue};
use crate::model::types::{Agent, AgentKind, Conversation, MessageRole};
use std::path::Path;

pub(super) fn fixture(data: &Path, parents: usize, per_parent: usize) -> Result<FrankenStorage> {
    let storage = FrankenStorage::open(&data.join("agent_search.db"))?;
    let agent = storage.ensure_agent(&Agent {
        id: None,
        slug: "codex".into(),
        name: "Codex".into(),
        version: None,
        kind: AgentKind::Cli,
    })?;
    for parent in 0..parents {
        let messages = (0..per_parent)
            .map(|ordinal| Message {
                id: None,
                idx: ordinal as i64,
                role: match ordinal % 4 {
                    0 => MessageRole::User,
                    1 => MessageRole::Agent,
                    2 => MessageRole::Tool,
                    _ => MessageRole::System,
                },
                author: Some("fixture".into()),
                created_at: Some(1_700_000_000_000 + ordinal as i64),
                content: format!("parent {parent} message {ordinal}: compiler ownership 東京"),
                extra_json: serde_json::json!({"raw_only": "not a semantic projection"}),
                snippets: Vec::new(),
            })
            .collect();
        storage.insert_conversation_tree(
            agent,
            None,
            &Conversation {
                id: None,
                agent_slug: "codex".into(),
                workspace: None,
                external_id: Some(format!("selected-{parent}")),
                title: Some("selected replay fixture".into()),
                source_path: format!("/selected-proof/{parent}.jsonl").into(),
                started_at: Some(1),
                ended_at: Some(2),
                approx_tokens: None,
                metadata_json: serde_json::Value::Null,
                messages,
                source_id: "local".into(),
                origin_host: None,
            },
        )?;
    }
    Ok(storage)
}

pub(super) fn coordinates(storage: &FrankenStorage) -> Result<Vec<(i64, i64, i64)>> {
    Ok(storage.raw().query_map_collect(
        "SELECT id, conversation_id, idx FROM messages ORDER BY conversation_id, idx, id",
        &[] as &[ParamValue],
        |row| Ok((row.get_typed(0)?, row.get_typed(1)?, row.get_typed(2)?)),
    )?)
}

pub(super) fn signature(inputs: Vec<engine::EmbeddingInput>) -> Vec<(Option<String>, String)> {
    inputs
        .into_iter()
        .map(|input| (engine::semantic_doc_id_for_input(&input), input.content))
        .collect()
}

#[test]
fn selected_projection_matches_incumbent_across_id_and_parent_pages() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = fixture(temp.path(), PARENT_PAGE_SIZE + 3, 2)?;
    let coordinates = coordinates(&storage)?;
    let parents: Vec<_> = coordinates.iter().rev().map(|row| row.1).collect();
    let ids: HashSet<_> = coordinates.iter().map(|row| row.0).collect();
    let expected =
        engine::packet_embedding_inputs_from_storage_for_message_ids(&storage, &parents, &ids)?;
    let selected = select_ids(storage.raw(), &parents, &ids)?;
    let mut loaded = Vec::new();
    let (actual, raw_max) = project_selection(&storage, selected, |parent, key| {
        loaded.push((key.id, parent, key.idx));
        load_message(storage.raw(), parent, key)
    })?;
    assert_eq!(
        loaded, coordinates,
        "one point body read per selected message"
    );
    assert_eq!(raw_max, coordinates.iter().map(|row| row.0).max());
    assert_eq!(signature(actual), signature(expected));
    storage.close()?;
    Ok(())
}

#[test]
fn selected_facade_does_not_decode_unrequested_history() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = fixture(temp.path(), 1, pages::MAX_ROWS + 2)?;
    let rows = coordinates(&storage)?;
    let (wanted, parent, _) = *rows.last().unwrap();
    storage.raw().execute_compat(
        "UPDATE messages SET role = X'00' WHERE id = ?1",
        &[ParamValue::from(rows[0].0)],
    )?;
    let ids = HashSet::from([wanted]);
    assert!(
        engine::packet_embedding_inputs_from_storage_for_message_ids(&storage, &[parent], &ids,)
            .is_err(),
        "incumbent eagerly decodes the rejected history"
    );
    // This calls the real facade, proving its explicit export uses the new path.
    let inputs = crate::indexer::semantic::packet_embedding_inputs_from_storage_for_message_ids(
        &storage,
        &[parent],
        &ids,
    )?;
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0].message_id, wanted as u64);
    storage.close()?;
    Ok(())
}

#[test]
fn explicit_parent_and_message_scopes_are_intersected_before_body_reads() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = fixture(temp.path(), 3, 3)?;
    let rows = coordinates(&storage)?;
    let parents = [rows[3].1, rows[0].1, rows[0].1, -999];
    let wanted = HashSet::from([rows[2].0, rows[3].0, rows[8].0, i64::MAX, i64::MIN]);
    storage.raw().execute_compat(
        "UPDATE messages SET role = X'00' WHERE id = ?1",
        &[ParamValue::from(rows[8].0)],
    )?;
    let selected = select_ids(storage.raw(), &parents, &wanted)?;
    let mut loaded = Vec::new();
    let (inputs, _) = project_selection(&storage, selected, |parent, key| {
        loaded.push((key.id, parent, key.idx));
        load_message(storage.raw(), parent, key)
    })?;
    assert_eq!(loaded, vec![rows[2], rows[3]]);
    assert_eq!(
        inputs
            .iter()
            .map(|input| input.message_id)
            .collect::<Vec<_>>(),
        vec![rows[2].0 as u64, rows[3].0 as u64]
    );
    storage.close()?;
    Ok(())
}

#[test]
fn selected_oversized_unicode_preserves_full_body_provenance_and_passages() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = fixture(temp.path(), 1, 3)?;
    let rows = coordinates(&storage)?;
    let (id, parent, idx) = rows[1];
    let long = "Unicode cafe\u{301} 東京 compiler ownership\n".repeat(32_768);
    assert!(long.len() > pages::MAX_CONTENT_BYTES);
    storage.raw().execute_compat(
        "UPDATE messages SET content = ?1, role = 'assistant', created_at = NULL WHERE id = ?2",
        &[ParamValue::from(long.as_str()), ParamValue::from(id)],
    )?;
    storage.raw().execute_compat(
        "UPDATE conversations SET origin_host = 'FixtureHost' WHERE id = ?1",
        &[ParamValue::from(parent)],
    )?;
    assert_eq!(
        load_message(storage.raw(), parent, MessageKey { id, idx })?.content,
        long
    );
    let ids = HashSet::from([id]);
    let expected =
        engine::packet_embedding_inputs_from_storage_for_message_ids(&storage, &[parent], &ids)?;
    let selected = select_ids(storage.raw(), &[parent], &ids)?;
    let (actual, _) = project_selection(&storage, selected, |parent, key| {
        load_message(storage.raw(), parent, key)
    })?;
    assert!(
        actual.len() > 1,
        "long selected text must exercise passage encoding"
    );
    assert_eq!(signature(actual), signature(expected));
    storage.close()?;
    Ok(())
}

#[test]
fn selected_identity_changes_and_decode_errors_reject_the_whole_batch() -> Result<()> {
    for move_order_key in [false, true] {
        let temp = tempfile::tempdir()?;
        let storage = fixture(temp.path(), 1, 3)?;
        let rows = coordinates(&storage)?;
        let ids: HashSet<_> = rows.iter().map(|row| row.0).collect();
        let selected = select_ids(storage.raw(), &[rows[0].1], &ids)?;
        let mut loaded = Vec::new();
        let result = project_selection(&storage, selected, |parent, key| {
            loaded.push(key.id);
            if key.id == rows[1].0 {
                storage.raw().execute_compat(
                    if move_order_key {
                        "UPDATE messages SET idx = idx + 1000 WHERE id = ?1"
                    } else {
                        "UPDATE messages SET role = X'00' WHERE id = ?1"
                    },
                    &[ParamValue::from(key.id)],
                )?;
            }
            load_message(storage.raw(), parent, key)
        });
        let error = result.expect_err("failure must not return earlier inputs or cursor progress");
        assert!(error.downcast_ref::<FrankenError>().is_some(), "{error:#}");
        assert_eq!(loaded, vec![rows[0].0, rows[1].0]);
        storage.close()?;
    }
    Ok(())
}

#[test]
fn selected_replay_leaves_the_callers_read_only_transaction_open() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = fixture(temp.path(), 1, 2)?;
    let rows = coordinates(&storage)?;
    storage.raw().execute("PRAGMA query_only = ON")?;
    storage.raw().execute("BEGIN")?;
    let selected = select_ids(storage.raw(), &[rows[0].1], &HashSet::from([rows[1].0]))?;
    let (inputs, _) = project_selection(&storage, selected, |parent, key| {
        // All metadata/envelope cursors have finalized before the body loader.
        assert_eq!(coordinates(&storage)?.len(), 2);
        load_message(storage.raw(), parent, key)
    })?;
    assert_eq!(inputs.len(), 1);
    storage.raw().execute("COMMIT")?;
    storage.close_without_checkpoint()?;
    Ok(())
}

#[test]
fn empty_explicit_scopes_do_not_touch_a_missing_message_table() -> Result<()> {
    let connection = Connection::open(":memory:")?;
    assert!(select_ids(&connection, &[], &HashSet::from([1]))?.is_empty());
    assert!(select_ids(&connection, &[1], &HashSet::new())?.is_empty());
    connection.close_without_checkpoint()?;
    Ok(())
}
