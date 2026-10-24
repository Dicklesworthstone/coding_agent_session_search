//! Real-engine regressions for bounded coverage and early replay termination.
//! Fixtures are private databases; no stock SQLite or process-wide env changes.

use super::*;
use crate::franken_sync::{
    FrankenError,
    compat::{ConnectionExt, ParamValue},
};
use crate::indexer::semantic::{SemanticBackfillStoragePlan, SemanticIndexer};
use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use crate::search::semantic_manifest::{SemanticManifest, TierKind};
use std::cell::Cell;
use std::fs;
use std::io;
use std::path::Path;

fn minimal_database(path: &Path, indexed: bool) -> Result<Connection> {
    let connection = Connection::open(path.to_string_lossy().into_owned())?;
    connection.execute("CREATE TABLE conversations (id INTEGER PRIMARY KEY)")?;
    // Deliberately omit content and tail columns: the summary needs neither.
    let unique = if indexed {
        ", UNIQUE(conversation_id, idx)"
    } else {
        ""
    };
    connection.execute(&format!(
        "CREATE TABLE messages (id INTEGER PRIMARY KEY, conversation_id INTEGER, idx INTEGER{unique})"
    ))?;
    Ok(connection)
}

fn add_parent(connection: &Connection, id: i64, messages: i64) -> Result<()> {
    connection.execute_compat(
        "INSERT INTO conversations (id) VALUES (?1)",
        &[ParamValue::from(id)],
    )?;
    for idx in 0..messages {
        connection.execute_compat(
            "INSERT INTO messages (conversation_id, idx) VALUES (?1, ?2)",
            &[ParamValue::from(id), ParamValue::from(idx)],
        )?;
    }
    Ok(())
}

#[test]
fn coverage_pages_all_parent_ids_and_counts_each_nonempty_parent_once() -> Result<()> {
    for indexed in [true, false] {
        let temp = tempfile::tempdir()?;
        let connection = minimal_database(&temp.path().join("coverage.db"), indexed)?;
        connection.execute("BEGIN IMMEDIATE")?;
        for id in [i64::MIN, -7, 0, 100_000, i64::MAX] {
            add_parent(&connection, id, 3)?;
        }
        // Cross multiple pages, including gaps and parents without messages.
        for id in 1..=(PARENT_PAGE_SIZE as i64 * 3) {
            add_parent(&connection, id, 0)?;
        }
        // An orphan message cannot stand in for a canonical parent.
        connection.execute("INSERT INTO messages (conversation_id, idx) VALUES (99999, 0)")?;
        connection.execute("COMMIT")?;
        let mut polls = 0usize;
        let summary = nonempty_conversations(&connection, || {
            polls += 1;
            Ok(())
        })?;
        assert_eq!(summary, (5, i64::MAX));
        assert!(
            polls > PARENT_PAGE_SIZE * 3,
            "cancellation must be polled per parent"
        );
        connection.close_without_checkpoint()?;
    }
    Ok(())
}

#[test]
fn coverage_preserves_empty_and_all_negative_maximum_semantics() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let connection = minimal_database(&temp.path().join("negative.db"), true)?;
    assert_eq!(nonempty_conversations(&connection, || Ok(()))?, (0, 0));
    add_parent(&connection, i64::MIN, 2)?;
    add_parent(&connection, -9, 1)?;
    add_parent(&connection, 0, 0)?;
    add_parent(&connection, i64::MAX, 0)?;
    assert_eq!(nonempty_conversations(&connection, || Ok(()))?, (2, -9));
    connection.close_without_checkpoint()?;
    Ok(())
}

#[test]
fn a_full_final_page_ending_at_max_rowid_does_not_overflow_or_loop() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let connection = minimal_database(&temp.path().join("maximum.db"), true)?;
    connection.execute("BEGIN IMMEDIATE")?;
    for id in 1..PARENT_PAGE_SIZE as i64 {
        add_parent(&connection, id, 0)?;
    }
    add_parent(&connection, i64::MAX, 1)?;
    connection.execute("COMMIT")?;
    assert_eq!(
        nonempty_conversations(&connection, || Ok(()))?,
        (1, i64::MAX)
    );
    connection.close_without_checkpoint()?;
    Ok(())
}

#[test]
fn coverage_cancellation_never_returns_a_partial_summary() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let connection = minimal_database(&temp.path().join("cancel.db"), true)?;
    connection.execute("BEGIN IMMEDIATE")?;
    for id in 1..=(PARENT_PAGE_SIZE as i64 + 2) {
        add_parent(&connection, id, 1)?;
    }
    connection.execute("COMMIT")?;
    let mut polls = 0usize;
    let error = nonempty_conversations(&connection, || {
        polls += 1;
        if polls == 20 {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "coverage cancelled").into());
        }
        Ok(())
    })
    .expect_err("a cancelled partial scan is not an archive summary");
    assert_eq!(polls, 20);
    assert_eq!(
        error.downcast_ref::<io::Error>().unwrap().kind(),
        io::ErrorKind::Interrupted
    );
    assert_eq!(
        nonempty_conversations(&connection, || Ok(()))?,
        (PARENT_PAGE_SIZE as u64 + 2, PARENT_PAGE_SIZE as i64 + 2)
    );
    connection.close_without_checkpoint()?;
    Ok(())
}

#[test]
fn only_the_missing_index_hint_allows_legacy_fallback() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let connection = minimal_database(&temp.path().join("errors.db"), false)?;
    add_parent(&connection, 1, 1)?;
    assert_eq!(nonempty_conversations(&connection, || Ok(()))?, (1, 1));
    connection.execute("ALTER TABLE messages RENAME TO retained_messages")?;
    let error = nonempty_conversations(&connection, || Ok(()))
        .expect_err("missing canonical tables must not be converted to an empty summary");
    assert!(error.downcast_ref::<FrankenError>().is_some(), "{error:#}");
    connection.close_without_checkpoint()?;
    Ok(())
}

fn canonical_database(data: &Path) -> Result<FrankenStorage> {
    let storage = FrankenStorage::open(&data.join("agent_search.db"))?;
    let agent_id = storage.ensure_agent(&Agent {
        id: None,
        slug: "codex".into(),
        name: "Codex".into(),
        version: None,
        kind: AgentKind::Cli,
    })?;
    for ordinal in 1..=3 {
        storage.insert_conversation_tree(
            agent_id,
            None,
            &Conversation {
                id: None,
                agent_slug: "codex".into(),
                workspace: None,
                external_id: Some(format!("bounded-proof-{ordinal}")),
                title: None,
                source_path: format!("/bounded-proof/{ordinal}.jsonl").into(),
                started_at: Some(1),
                ended_at: Some(2),
                approx_tokens: None,
                metadata_json: serde_json::json!({}),
                source_id: "local".into(),
                origin_host: None,
                messages: vec![Message {
                    id: None,
                    idx: 0,
                    role: MessageRole::User,
                    author: None,
                    created_at: Some(1),
                    content: format!("compiler checkpoint ownership message {ordinal}"),
                    extra_json: serde_json::json!({}),
                    snippets: Vec::new(),
                }],
            },
        )?;
    }
    Ok(storage)
}

#[test]
fn canonical_coverage_ignores_stale_tail_caches_but_counts_blank_messages() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = canonical_database(temp.path())?;
    storage
        .raw()
        .execute("UPDATE messages SET content = '' WHERE id = 1")?;
    storage
        .raw()
        .execute("DELETE FROM messages WHERE conversation_id = 3")?;
    // The deletion deliberately leaves the ingestion-maintained tail cache.
    // A blank message still makes its parent nonempty for coverage purposes.
    assert_eq!(nonempty_conversations(storage.raw(), || Ok(()))?, (2, 2));
    storage.close()?;
    Ok(())
}

fn canonical_ids(storage: &FrankenStorage) -> Result<HashSet<String>> {
    Ok(engine::packet_embedding_inputs_from_storage(storage)?
        .iter()
        .filter_map(engine::semantic_doc_id_for_input)
        .collect())
}

#[test]
fn real_replay_stops_at_first_identity_delta_and_detects_deleted_coverage() -> Result<()> {
    for (mutation, visited) in [
        (
            "UPDATE messages SET content = 'changed first body' WHERE id = 1",
            1,
        ),
        ("UPDATE messages SET role = 'tool' WHERE id = 1", 1),
        (
            "UPDATE messages SET content = 'changed last body' WHERE id = 3",
            3,
        ),
        ("DELETE FROM messages WHERE id = 1", 2),
    ] {
        let temp = tempfile::tempdir()?;
        let storage = canonical_database(temp.path())?;
        let original = canonical_ids(&storage)?;
        assert_eq!(original.len(), 3);
        let mut remaining = original.iter().map(String::as_str).collect();
        let sink = SemanticProgressSink::disabled();
        assert_eq!(
            compare(&storage, &mut remaining, &sink, 3, || Ok(()))?,
            Comparison {
                matches: true,
                visited: 3
            }
        );
        assert!(remaining.is_empty());
        storage.raw().execute(mutation)?;
        let mut remaining = original.iter().map(String::as_str).collect();
        assert_eq!(
            compare(&storage, &mut remaining, &sink, 3, || Ok(()))?,
            Comparison {
                matches: false,
                visited
            }
        );
        assert!(!remaining.is_empty());
        storage.close()?;
    }
    Ok(())
}

#[test]
fn replay_cancellation_and_sql_failure_are_not_normal_mismatches() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = canonical_database(temp.path())?;
    let ids = canonical_ids(&storage)?;
    let mut remaining = ids.iter().map(String::as_str).collect();
    let sink = SemanticProgressSink::disabled();
    let mut polls = 0;
    let error = compare(&storage, &mut remaining, &sink, 3, || {
        polls += 1;
        if polls == 3 {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "replay cancelled").into());
        }
        Ok(())
    })
    .expect_err("cancellation is not a content mismatch");
    assert_eq!(
        error.downcast_ref::<io::Error>().unwrap().kind(),
        io::ErrorKind::Interrupted
    );
    assert_eq!(polls, 3);
    storage
        .raw()
        .execute("ALTER TABLE messages RENAME TO retained_messages")?;
    let mut remaining = ids.iter().map(String::as_str).collect();
    let error = compare(&storage, &mut remaining, &sink, 3, || Ok(()))
        .expect_err("a canonical SQL failure is not a normal cache miss");
    assert!(format!("{error:#}").contains("messages"), "{error:#}");
    storage.close_without_checkpoint()?;
    Ok(())
}

#[test]
fn early_mismatch_still_runs_final_fences_and_cannot_refresh_a_receipt() -> Result<()> {
    for change_during_checkpoint in [false, true] {
        let temp = tempfile::tempdir()?;
        let data = temp.path();
        let storage = canonical_database(data)?;
        let plan = SemanticBackfillStoragePlan {
            tier: TierKind::Quality,
            db_fingerprint: crate::indexer::lexical_storage_fingerprint_for_storage(&storage)?,
            model_revision: "hash".into(),
            max_conversations: 16,
        };
        let indexer = SemanticIndexer::new("hash", None)?;
        let mut manifest = SemanticManifest::default();
        let built =
            indexer.run_backfill_from_storage(&storage, data, &mut manifest, plan.clone())?;
        assert!(built.published);
        let receipt = data
            .join(crate::search::vector_index::VECTOR_INDEX_DIR)
            .join(".completed-backfill-quality-fnv1a-384.json");
        fs::write(&receipt, b"retained invalid cache")?;
        storage
            .raw()
            .execute("UPDATE messages SET content = 'delta before proof' WHERE id = 1")?;
        let vector_before = fs::read(&built.index_path)?;
        let manifest_before = fs::read(SemanticManifest::path(data))?;
        let reached = Cell::new(false);
        let inner = engine::SemanticIndexer::new("hash", None)?;
        let result = super::super::prove_unchanged(
            &inner,
            &storage,
            data,
            &manifest,
            &plan,
            &SemanticProgressSink::disabled(),
            || {
                reached.set(true);
                if change_during_checkpoint {
                    storage.raw().execute(
                        "UPDATE messages SET content = 'delta during proof' WHERE id = 2",
                    )?;
                }
                Ok(())
            },
        );
        assert!(
            reached.get(),
            "early termination must not bypass the final proof checkpoint"
        );
        if change_during_checkpoint {
            let error = result.expect_err("a changing archive must not reach writer fallback");
            assert!(format!("{error:#}").contains("changed during unchanged proof"));
        } else {
            assert!(result?.is_none());
        }
        assert_eq!(fs::read(&built.index_path)?, vector_before);
        assert_eq!(fs::read(SemanticManifest::path(data))?, manifest_before);
        assert_eq!(fs::read(receipt)?, b"retained invalid cache");
        storage.close()?;
    }
    Ok(())
}
