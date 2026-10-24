//! Lazy admission uses real FrankenSQLite rows and the established packet encoder.
//! Loader instrumentation observes actual body reads, never substitutes a DB.

use super::*;
use crate::indexer::semantic_progress::SemanticProgressSink;
use crate::model::types::{Agent, AgentKind, MessageRole};
use std::cell::Cell;
use std::io;
use std::path::Path;

fn storage(data: &Path, count: usize) -> Result<FrankenStorage> {
    let storage = FrankenStorage::open(&data.join("agent_search.db"))?;
    let agent = storage.ensure_agent(&Agent {
        id: None,
        slug: "codex".into(),
        name: "Codex".into(),
        version: None,
        kind: AgentKind::Cli,
    })?;
    let workspace = storage.ensure_workspace(Path::new("/lazy-proof/workspace"), None)?;
    for ordinal in 0..count {
        storage.insert_conversation_tree(
            agent,
            Some(workspace),
            &Conversation {
                id: None,
                agent_slug: "codex".into(),
                workspace: Some(PathBuf::from("/lazy-proof/workspace")),
                external_id: Some(format!("lazy-{ordinal}")),
                title: Some(format!("lazy canonical proof {ordinal}")),
                source_path: format!("/lazy-proof/{ordinal}.jsonl").into(),
                started_at: Some(1),
                ended_at: Some(2),
                approx_tokens: None,
                metadata_json: serde_json::Value::Null,
                source_id: "local".into(),
                origin_host: None,
                messages: vec![Message {
                    id: None,
                    idx: 0,
                    role: MessageRole::User,
                    author: None,
                    created_at: Some(1_700_000_000_000 + ordinal as i64),
                    content: format!("canonical compiler ownership {ordinal}"),
                    extra_json: serde_json::Value::Null,
                    snippets: Vec::new(),
                }],
            },
        )?;
    }
    Ok(storage)
}

fn ids(storage: &FrankenStorage) -> Result<Vec<i64>> {
    Ok(storage.raw().query_map_collect(
        "SELECT id FROM conversations ORDER BY id",
        &[] as &[ParamValue],
        |row| row.get_typed(0),
    )?)
}

fn signature(inputs: Vec<engine::EmbeddingInput>) -> Vec<(Option<String>, String)> {
    inputs
        .into_iter()
        .map(|input| (engine::semantic_doc_id_for_input(&input), input.content))
        .collect()
}

#[test]
fn lazy_replay_matches_existing_packet_replay_across_pages_and_passages() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = storage(temp.path(), PAGE_SIZE + 3)?;
    let parents = ids(&storage)?;
    // Exercise content chunking, roles, Unicode and provenance, not only counts.
    storage.raw().execute_compat(
        "UPDATE messages SET content = ?1, role = 'tool', author = 'fixture' WHERE conversation_id = ?2",
        &[ParamValue::from("Unicode cafe\u{301} 東京 compiler ownership ".repeat(400)), ParamValue::from(parents[0])],
    )?;
    storage.raw().execute_compat(
        "UPDATE conversations SET origin_host = 'FixtureHost' WHERE id = ?1",
        &[ParamValue::from(parents[0])],
    )?;
    storage.raw().execute_compat(
        "UPDATE messages SET content = '', role = 'system' WHERE conversation_id = ?1",
        &[ParamValue::from(parents[1])],
    )?;
    let incumbent = engine::packet_embedding_inputs_from_storage(&storage)?;
    let mut actual = Vec::new();
    let mut loaded = Vec::new();
    visit_with_loader(
        &storage,
        |id| {
            loaded.push(id);
            messages(&storage, id)
        },
        |input| {
            actual.push(input);
            Ok(())
        },
        || Ok(()),
    )?;
    assert_eq!(
        loaded, parents,
        "each body is loaded once, in canonical parent order"
    );
    assert!(
        actual.len() > parents.len(),
        "the long message must produce multiple passages"
    );
    assert_eq!(signature(actual), signature(incumbent));
    storage.close()?;
    Ok(())
}

#[derive(Debug, thiserror::Error)]
#[error("consumer deliberately stopped canonical replay")]
struct Stop;

#[test]
fn first_delta_stops_before_loading_other_bodies_in_the_same_page() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = storage(temp.path(), 3)?;
    let parents = ids(&storage)?;
    let mut loaded = Vec::new();
    let mut seen = 0;
    let error = visit_with_loader(
        &storage,
        |id| {
            loaded.push(id);
            messages(&storage, id)
        },
        |_| {
            seen += 1;
            Err(Stop.into())
        },
        || Ok(()),
    )
    .expect_err("the consumer's stop must propagate");
    assert!(error.is::<Stop>());
    assert_eq!(seen, 1);
    assert_eq!(loaded, parents[..1]);
    // The production comparison uses the lazy visitor, retaining its exact
    // mismatch classification and duplicate/deletion accounting.
    let mut remaining = std::collections::HashSet::new();
    let result = super::super::compare(
        &storage,
        &mut remaining,
        &SemanticProgressSink::disabled(),
        0,
        || Ok(()),
    )?;
    assert!(!result.matches);
    assert_eq!(result.visited, 1);
    storage.close()?;
    Ok(())
}

#[test]
fn cancellation_before_body_admission_never_loads_a_conversation() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = storage(temp.path(), 3)?;
    for threshold in [1, 2, 3] {
        let mut polls = 0;
        let mut loaded = 0;
        let error = visit_with_loader(
            &storage,
            |id| {
                loaded += 1;
                messages(&storage, id)
            },
            |_| Ok(()),
            || {
                polls += 1;
                if polls == threshold {
                    return Err(
                        io::Error::new(io::ErrorKind::Interrupted, "cancel admission").into(),
                    );
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(loaded, 0);
        assert_eq!(polls, threshold);
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::Interrupted
        );
    }
    storage.close()?;
    Ok(())
}

#[test]
fn cancellation_after_first_input_does_not_load_the_next_body() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = storage(temp.path(), 3)?;
    let parents = ids(&storage)?;
    let stopped = Cell::new(false);
    let mut loaded = Vec::new();
    let mut seen = 0;
    let error = visit_with_loader(
        &storage,
        |id| {
            loaded.push(id);
            messages(&storage, id)
        },
        |_| {
            seen += 1;
            stopped.set(true);
            Ok(())
        },
        || {
            if stopped.get() {
                return Err(Stop.into());
            }
            Ok(())
        },
    )
    .unwrap_err();
    assert!(error.is::<Stop>());
    assert_eq!(seen, 1);
    assert_eq!(loaded, parents[..1]);
    storage.close()?;
    Ok(())
}

#[test]
fn a_body_read_failure_remains_typed_and_stops_further_admission() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = storage(temp.path(), 3)?;
    let parents = ids(&storage)?;
    let mut loaded = Vec::new();
    let mut seen = 0;
    let error = visit_with_loader(
        &storage,
        |id| {
            loaded.push(id);
            if id == parents[1] {
                return Err(
                    io::Error::new(io::ErrorKind::PermissionDenied, "body inaccessible").into(),
                );
            }
            messages(&storage, id)
        },
        |_| {
            seen += 1;
            Ok(())
        },
        || Ok(()),
    )
    .expect_err("a body failure must not become successful partial replay");
    assert_eq!(loaded, parents[..2]);
    assert_eq!(seen, 1);
    assert_eq!(
        error.downcast_ref::<io::Error>().unwrap().kind(),
        io::ErrorKind::PermissionDenied
    );
    storage.close()?;
    Ok(())
}

#[test]
fn signed_parent_ids_are_replayed_without_tail_metadata_or_cursor_overflow() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = storage(temp.path(), 0)?;
    let agent: i64 = storage.raw().query_row_map(
        "SELECT id FROM agents WHERE slug = 'codex'",
        &[] as &[ParamValue],
        |row| row.get_typed(0),
    )?;
    let parents = [i64::MIN, -5, 0, i64::MAX];
    storage.raw().execute("BEGIN IMMEDIATE")?;
    for (ordinal, id) in parents.iter().copied().enumerate() {
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
            "INSERT INTO messages(conversation_id, idx, role, content, created_at)
             VALUES (?1, 0, 'user', ?2, 1)",
            &[
                ParamValue::from(id),
                ParamValue::from(format!("signed canonical parent {ordinal}")),
            ],
        )?;
    }
    storage.raw().execute("COMMIT")?;
    let mut loaded = Vec::new();
    let mut actual = Vec::new();
    visit_with_loader(
        &storage,
        |id| {
            loaded.push(id);
            messages(&storage, id)
        },
        |input| {
            actual.push(input);
            Ok(())
        },
        || Ok(()),
    )?;
    assert_eq!(loaded, parents);
    assert_eq!(actual.len(), parents.len());
    for (ordinal, input) in actual.iter().enumerate() {
        assert_eq!(input.content, format!("signed canonical parent {ordinal}"));
        assert!(engine::semantic_doc_id_for_input(input).is_some());
    }
    storage.close()?;
    Ok(())
}

#[test]
fn empty_and_deleted_conversations_advance_without_inventing_inputs() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = storage(temp.path(), 0)?;
    let mut loaded = 0;
    visit_with_loader(
        &storage,
        |_| {
            loaded += 1;
            Ok(Vec::new())
        },
        |_| Err(Stop.into()),
        || Ok(()),
    )?;
    assert_eq!(loaded, 0);
    storage.close()?;

    let temp = tempfile::tempdir()?;
    let storage = self::storage(temp.path(), PAGE_SIZE + 1)?;
    let parents = ids(&storage)?;
    // Deliberately retain stale ingest-tail rows after deleting canonical bodies.
    storage.raw().execute("DELETE FROM messages")?;
    let mut loaded = Vec::new();
    visit_with_loader(
        &storage,
        |id| {
            loaded.push(id);
            messages(&storage, id)
        },
        |_| Err(Stop.into()),
        || Ok(()),
    )?;
    assert_eq!(loaded, parents);
    storage.close()?;
    Ok(())
}

mod coverage;
