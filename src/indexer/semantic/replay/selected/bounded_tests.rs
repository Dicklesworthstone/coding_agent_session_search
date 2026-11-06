//! Admission happens on real canonical rows before inference, and an
//! oversized selection never exposes a prefix as a complete source boundary.
use super::tests::{coordinates, fixture, signature};
use super::*;
use crate::franken_sync::compat::{ConnectionExt, ParamValue};

fn limits() -> SemanticDeltaSelectionLimits {
    SemanticDeltaSelectionLimits {
        max_messages: 4096,
        max_inputs: 4096,
        max_content_bytes: 1024 * 1024,
    }
}

fn complete(selection: SemanticDeltaSelection) -> engine::CanonicalIncrementalEmbeddingBatch {
    match selection {
        SemanticDeltaSelection::Complete(batch) => batch,
        SemanticDeltaSelection::Reconcile => panic!("small complete delta must use fast append"),
    }
}

#[test]
fn bounded_delta_preserves_complete_projection_and_raw_progress() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = fixture(temp.path(), 3, pages::MAX_ROWS + 7)?;
    let rows = coordinates(&storage)?;
    let since = rows[20].0;
    storage
        .raw()
        .execute("UPDATE messages SET idx = 1000000 - idx")?;
    storage.raw().execute_compat(
        "INSERT INTO messages(conversation_id, idx, role, content, created_at)
         VALUES (?1, 1001, 'assistant', 'new append to an older parent', 1)",
        &[ParamValue::from(rows[0].1)],
    )?;
    let expected = engine::packet_embedding_inputs_from_storage_since(&storage, since)?;
    let actual = complete(packet_embedding_inputs_from_storage_since_bounded(
        &storage,
        since,
        limits(),
    )?);
    assert_eq!(actual.raw_max_message_id, expected.raw_max_message_id);
    assert_eq!(
        actual.conversations_in_batch,
        expected.conversations_in_batch
    );
    assert_eq!(signature(actual.inputs), signature(expected.inputs));
    storage.close()?;
    Ok(())
}

#[test]
fn bounded_delta_coordinate_overflow_never_decodes_any_message_body() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = fixture(temp.path(), 2, pages::MAX_ROWS + 3)?;
    storage.raw().execute("UPDATE messages SET role = X'00'")?;
    let mut bound = limits();
    bound.max_messages = pages::MAX_ROWS + 1;
    assert!(matches!(
        packet_embedding_inputs_from_storage_since_bounded(&storage, 0, bound)?,
        SemanticDeltaSelection::Reconcile
    ));
    assert!(
        engine::packet_embedding_inputs_from_storage_since(&storage, 0).is_err(),
        "negative control: eager selection tries to decode the malformed bodies"
    );
    storage.close()?;
    Ok(())
}

#[test]
fn bounded_delta_exact_input_boundary_is_admitted_and_overflow_is_not_a_prefix() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = fixture(temp.path(), 1, 3)?;
    let mut bound = limits();
    bound.max_messages = 3;
    bound.max_inputs = 3;
    let batch = complete(packet_embedding_inputs_from_storage_since_bounded(
        &storage, 0, bound,
    )?);
    assert_eq!(batch.inputs.len(), 3);
    assert_eq!(batch.raw_max_message_id, Some(coordinates(&storage)?[2].0));
    bound.max_inputs = 2;
    assert!(matches!(
        packet_embedding_inputs_from_storage_since_bounded(&storage, 0, bound)?,
        SemanticDeltaSelection::Reconcile
    ));
    bound.max_inputs = 3;
    bound.max_messages = 2;
    assert!(matches!(
        packet_embedding_inputs_from_storage_since_bounded(&storage, 0, bound)?,
        SemanticDeltaSelection::Reconcile
    ));
    storage.close()?;
    Ok(())
}

#[test]
fn bounded_delta_limits_projected_passages_not_only_raw_message_count() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = fixture(temp.path(), 1, 1)?;
    storage.raw().execute_compat(
        "UPDATE messages SET content = ?1",
        &[ParamValue::from(
            "Unicode compiler ownership 東京\n".repeat(4096),
        )],
    )?;
    let expected = engine::packet_embedding_inputs_from_storage_since(&storage, 0)?;
    assert!(expected.inputs.len() > 1);
    let mut bound = limits();
    bound.max_messages = 1;
    bound.max_inputs = 1;
    assert!(matches!(
        packet_embedding_inputs_from_storage_since_bounded(&storage, 0, bound)?,
        SemanticDeltaSelection::Reconcile
    ));
    storage.close()?;
    Ok(())
}

#[test]
fn bounded_delta_stops_projecting_when_retained_text_budget_is_full() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = fixture(temp.path(), 1, 3)?;
    let rows = coordinates(&storage)?;
    let body = "compiler ownership proof ".repeat(4);
    assert!(body.len() < 150 && body.len() * 2 > 150);
    storage.raw().execute_compat(
        "UPDATE messages SET content = ?1",
        &[ParamValue::from(body)],
    )?;
    storage.raw().execute_compat(
        "UPDATE messages SET role = X'00' WHERE id = ?1",
        &[ParamValue::from(rows[2].0)],
    )?;
    let mut bound = limits();
    bound.max_content_bytes = 150;
    assert!(matches!(
        packet_embedding_inputs_from_storage_since_bounded(&storage, 0, bound)?,
        SemanticDeltaSelection::Reconcile
    ));
    assert!(
        engine::packet_embedding_inputs_from_storage_since(&storage, 0).is_err(),
        "negative control: collecting the tail reaches the later corrupt body"
    );
    storage.close()?;
    Ok(())
}

#[test]
fn bounded_delta_oversized_single_body_is_not_expanded_into_all_passages() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = fixture(temp.path(), 1, 2)?;
    let rows = coordinates(&storage)?;
    storage.raw().execute_compat(
        "UPDATE messages SET content = ?1 WHERE id = ?2",
        &[
            ParamValue::from("Unicode compiler ownership 東京 ".repeat(4096)),
            ParamValue::from(rows[0].0),
        ],
    )?;
    storage.raw().execute_compat(
        "UPDATE messages SET role = X'00' WHERE id = ?1",
        &[ParamValue::from(rows[1].0)],
    )?;
    let selected = select_since(storage.raw(), 0)?;
    let mut bound = limits();
    bound.max_content_bytes = 1024;
    let mut loaded = Vec::new();
    let result = project_selection_with_limits(&storage, selected, Some(bound), |parent, key| {
        loaded.push(key.id);
        load_message(storage.raw(), parent, key)
    })?;
    assert!(result.is_none());
    assert_eq!(loaded, vec![rows[0].0]);
    storage.close()?;
    Ok(())
}

#[test]
fn bounded_delta_preserves_blank_progress_even_with_zero_vector_allowance() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = fixture(temp.path(), 2, 3)?;
    storage.raw().execute("UPDATE messages SET content = ''")?;
    let mut bound = limits();
    bound.max_inputs = 0;
    bound.max_content_bytes = 0;
    let batch = complete(packet_embedding_inputs_from_storage_since_bounded(
        &storage, 0, bound,
    )?);
    assert!(batch.inputs.is_empty());
    assert_eq!(batch.conversations_in_batch, 2);
    let high_water = coordinates(&storage)?.last().unwrap().0;
    assert_eq!(batch.raw_max_message_id, Some(high_water));
    let empty = complete(packet_embedding_inputs_from_storage_since_bounded(
        &storage, high_water, bound,
    )?);
    assert!(empty.inputs.is_empty());
    assert_eq!(empty.raw_max_message_id, None);
    storage.close()?;
    Ok(())
}

#[test]
fn bounded_delta_keeps_corruption_and_disappearing_endpoints_as_errors() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = fixture(temp.path(), 1, 2)?;
    storage.raw().execute("UPDATE messages SET role = X'00'")?;
    let error = match packet_embedding_inputs_from_storage_since_bounded(&storage, 0, limits()) {
        Ok(_) => panic!("malformed selected data must not become an oversize result"),
        Err(error) => error,
    };
    assert!(error.downcast_ref::<FrankenError>().is_some());
    let high_water = coordinates(&storage)?.last().unwrap().0;
    assert!(select_since_through_with_limit(storage.raw(), 0, high_water + 1, Some(3)).is_err());
    storage.close()?;
    Ok(())
}
