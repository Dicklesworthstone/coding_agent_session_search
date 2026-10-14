//! Catch-up exercises the actual selector, body loader, packet projection and
//! facade API. The legacy engine supplies the independent output oracle.
use super::tests::{coordinates, fixture, signature};
use super::*;
use crate::franken_sync::compat::{ConnectionExt, ParamValue};

#[test]
fn catch_up_matches_incumbent_in_parent_order_not_message_id_order() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = fixture(temp.path(), 3, pages::MAX_ROWS + 7)?;
    let rows = coordinates(&storage)?;
    let since = rows[20].0;
    // Ordering keys may be unrelated to global IDs, including on an older
    // parent receiving a newer append. Neither order is a safe ID cursor.
    storage
        .raw()
        .execute("UPDATE messages SET idx = 1000000 - idx")?;
    storage.raw().execute_compat(
        "INSERT INTO messages(conversation_id, idx, role, content, created_at)
         VALUES (?1, 1000, 'assistant', 'new append to an older parent', 1)",
        &[ParamValue::from(rows[0].1)],
    )?;
    let expected = engine::packet_embedding_inputs_from_storage_since(&storage, since)?;
    let actual =
        crate::indexer::semantic::packet_embedding_inputs_from_storage_since(&storage, since)?;
    assert_eq!(
        actual.conversations_in_batch,
        expected.conversations_in_batch
    );
    assert_eq!(actual.raw_max_message_id, expected.raw_max_message_id);
    assert_eq!(signature(actual.inputs), signature(expected.inputs));
    let selected = select_since(storage.raw(), since)?;
    let mut loaded = Vec::new();
    project_selection(&storage, selected, |parent, key| {
        loaded.push((key.id, parent, key.idx));
        load_message(storage.raw(), parent, key)
    })?;
    let expected: Vec<_> = coordinates(&storage)?
        .into_iter()
        .filter(|row| row.0 > since)
        .collect();
    assert_eq!(
        loaded, expected,
        "every changed body is loaded once, and no old body is loaded"
    );
    storage.close()?;
    Ok(())
}

#[test]
fn catch_up_ignores_malformed_old_history_but_never_malformed_new_rows() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = fixture(temp.path(), 1, pages::MAX_ROWS + 3)?;
    let rows = coordinates(&storage)?;
    let since = rows[rows.len() - 2].0;
    storage.raw().execute_compat(
        "UPDATE messages SET role = X'00' WHERE id = ?1",
        &[ParamValue::from(rows[0].0)],
    )?;
    assert!(engine::packet_embedding_inputs_from_storage_since(&storage, since).is_err());
    let actual =
        crate::indexer::semantic::packet_embedding_inputs_from_storage_since(&storage, since)?;
    assert_eq!(actual.inputs.len(), 1);
    assert_eq!(actual.inputs[0].message_id, rows.last().unwrap().0 as u64);
    assert_eq!(actual.raw_max_message_id, Some(rows.last().unwrap().0));
    storage.raw().execute_compat(
        "UPDATE messages SET role = X'00' WHERE id = ?1",
        &[ParamValue::from(rows.last().unwrap().0)],
    )?;
    let error = match packet_embedding_inputs_from_storage_since(&storage, since) {
        Ok(_) => panic!("a malformed selected row must not acknowledge a cursor"),
        Err(error) => error,
    };
    assert!(error.downcast_ref::<FrankenError>().is_some());
    storage.close()?;
    Ok(())
}

#[test]
fn blank_delta_advances_the_raw_cursor_without_inventing_embeddings() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = fixture(temp.path(), 2, 3)?;
    let rows = coordinates(&storage)?;
    let since = rows[0].0;
    storage.raw().execute_compat(
        "UPDATE messages SET content = '' WHERE id > ?1",
        &[ParamValue::from(since)],
    )?;
    let actual = packet_embedding_inputs_from_storage_since(&storage, since)?;
    assert!(actual.inputs.is_empty());
    assert_eq!(actual.conversations_in_batch, 2);
    let high_water = rows.last().unwrap().0;
    assert_eq!(actual.raw_max_message_id, Some(high_water));
    let next = packet_embedding_inputs_from_storage_since(&storage, high_water)?;
    assert!(next.inputs.is_empty());
    assert_eq!(next.conversations_in_batch, 0);
    assert_eq!(next.raw_max_message_id, None);
    storage.close()?;
    Ok(())
}

#[test]
fn fixed_high_water_defers_a_later_append_without_losing_it() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = fixture(temp.path(), 2, pages::MAX_ROWS + 1)?;
    let rows = coordinates(&storage)?;
    let high_water = rows.iter().map(|row| row.0).max().unwrap();
    storage.raw().execute_compat(
        "INSERT INTO messages(conversation_id, idx, role, content, created_at)
         VALUES (?1, 1000, 'tool', 'arrived after captured high water', 1)",
        &[ParamValue::from(rows[0].1)],
    )?;
    let new_id = storage.raw().last_insert_rowid();
    assert!(new_id > high_water);
    let selected = select_since_through(storage.raw(), 0, high_water)?;
    let mut loaded = Vec::new();
    let (_, raw_max) = project_selection(&storage, selected, |parent, key| {
        loaded.push(key.id);
        load_message(storage.raw(), parent, key)
    })?;
    assert_eq!(loaded.len(), rows.len());
    assert!(!loaded.contains(&new_id));
    assert_eq!(raw_max, Some(high_water));
    let next = packet_embedding_inputs_from_storage_since(&storage, high_water)?;
    assert_eq!(next.inputs.len(), 1);
    assert_eq!(next.inputs[0].message_id, new_id as u64);
    assert_eq!(next.raw_max_message_id, Some(new_id));
    storage.close()?;
    Ok(())
}

#[test]
fn catch_up_identity_pages_handle_signed_keys_and_i64_max_without_overflow() -> Result<()> {
    let connection = Connection::open(":memory:")?;
    connection.execute(
        "CREATE TABLE messages(id INTEGER PRIMARY KEY, conversation_id INTEGER, idx INTEGER)",
    )?;
    let ids = [i64::MIN, -9, 0, 3, i64::MAX];
    for (ordinal, id) in ids.into_iter().enumerate() {
        connection.execute_compat(
            "INSERT INTO messages VALUES (?1, ?2, ?3)",
            &[
                ParamValue::from(id),
                ParamValue::from(ordinal as i64 - 2),
                ParamValue::from(-1i64),
            ],
        )?;
    }
    for since in [i64::MIN, -10, -1, 0, 3, i64::MAX] {
        let selected = select_since(&connection, since)?;
        let mut found: Vec<_> = selected.values().flatten().map(|key| key.id).collect();
        found.sort_unstable();
        assert_eq!(
            found,
            ids.into_iter().filter(|id| *id > since).collect::<Vec<_>>()
        );
    }
    assert!(
        select_since_through(&connection, 0, 4).is_err(),
        "a disappearing captured endpoint must not certify a partial range"
    );
    connection.close_without_checkpoint()?;
    Ok(())
}

#[test]
fn catch_up_missing_parent_rejects_the_batch_before_returning_cursor_progress() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = fixture(temp.path(), 1, 2)?;
    let selected = select_since(storage.raw(), 0)?;
    // Simulate a parent disappearing between selection and envelope loading
    // without changing/deleting canonical fixture data.
    let keys: Vec<_> = selected.into_values().flatten().collect();
    let selected = Selection::from([(i64::MAX, keys)]);
    let mut loads = 0;
    assert!(
        project_selection(&storage, selected, |parent, key| {
            loads += 1;
            load_message(storage.raw(), parent, key)
        })
        .is_err()
    );
    assert_eq!(loads, 0);
    storage.close()?;
    Ok(())
}

#[test]
fn negative_raw_message_ids_keep_progress_even_when_projection_skips_them() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = fixture(temp.path(), 1, 1)?;
    let row = coordinates(&storage)?[0];
    storage.raw().execute_compat(
        "UPDATE messages SET id = -5 WHERE id = ?1",
        &[ParamValue::from(row.0)],
    )?;
    let actual = packet_embedding_inputs_from_storage_since(&storage, i64::MIN)?;
    assert!(actual.inputs.is_empty());
    assert_eq!(actual.conversations_in_batch, 1);
    assert_eq!(actual.raw_max_message_id, Some(-5));
    storage.close()?;
    Ok(())
}
