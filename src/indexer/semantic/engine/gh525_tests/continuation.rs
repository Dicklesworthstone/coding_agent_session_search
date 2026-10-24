//! Checkpoint continuation and failure regressions for GH #525.
//! These tests call the production selector; storage cases use FrankenSQLite.

use super::*;

#[test]
fn inspected_empty_or_filtered_pages_advance_without_claiming_documents() -> Result<()> {
    for cutoff in [None, Some(i64::MAX)] {
        let mut reads = Vec::new();
        let (selection, grouped) = select_checkpoint_capped_conversations_lazily(
            (4..=6).map(conversation).collect(),
            cutoff,
            SemanticCheckpointCaps::unlimited(),
            |id| {
                reads.push(id);
                Ok(if cutoff.is_some() {
                    vec![message(id, 4)]
                } else {
                    Vec::new()
                })
            },
        )?;
        assert_eq!(reads, [4, 5, 6]);
        assert!(selection.conversations.is_empty());
        assert!(grouped.is_empty());
        assert_eq!(selection.last_conversation_id, Some(6));
        assert!(!selection.stopped_before_candidate);
    }
    Ok(())
}

#[test]
fn inspected_empty_cursor_never_acknowledges_an_over_budget_or_unread_body() -> Result<()> {
    let mut reads = Vec::new();
    let (selection, grouped) = select_checkpoint_capped_conversations_lazily(
        (1..=4).map(conversation).collect(),
        None,
        SemanticCheckpointCaps {
            max_messages: 2,
            max_bytes: 0,
        },
        |id| {
            reads.push(id);
            Ok(match id {
                1 => vec![message(10, 4)],
                2 => Vec::new(),
                3 => vec![message(30, 4), message(31, 4)],
                _ => panic!("a rejected nonempty candidate must stop further loading"),
            })
        },
    )?;
    assert_eq!(reads, [1, 2, 3]);
    assert_eq!(selection.last_conversation_id, Some(2));
    assert_eq!(selection.conversations.len(), 1);
    assert!(selection.stopped_before_candidate);
    assert_eq!(grouped.len(), 1);
    assert!(grouped.contains_key(&1));

    let (full, _) = select_checkpoint_capped_conversations_lazily(
        vec![conversation(1), conversation(2)],
        None,
        SemanticCheckpointCaps {
            max_messages: 1,
            max_bytes: 0,
        },
        |id| {
            assert_eq!(id, 1, "even an apparently empty future candidate is unread");
            Ok(vec![message(id, 1)])
        },
    )?;
    assert_eq!(full.last_conversation_id, Some(1));
    assert!(full.stopped_before_candidate);
    Ok(())
}

#[test]
fn loader_failure_aborts_the_batch_and_preserves_its_error_type() {
    let mut reads = Vec::new();
    let error = select_checkpoint_capped_conversations_lazily(
        (1..=3).map(conversation).collect(),
        None,
        SemanticCheckpointCaps::unlimited(),
        |id| {
            reads.push(id);
            if id == 2 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "body denied",
                )
                .into());
            }
            Ok(vec![message(id, 1)])
        },
    )
    .err()
    .expect("an I/O failure is not a partial successful checkpoint");
    assert_eq!(reads, [1, 2]);
    assert_eq!(
        error.downcast_ref::<std::io::Error>().unwrap().kind(),
        std::io::ErrorKind::PermissionDenied
    );
}

#[test]
fn an_unread_failure_is_deferred_until_resume_not_reported_as_exhaustion() -> Result<()> {
    let mut load = |id| {
        if id == 2 {
            bail!("later unread body failed");
        }
        Ok(vec![message(id, 1)])
    };
    let caps = SemanticCheckpointCaps {
        max_messages: 1,
        max_bytes: 0,
    };
    let (first, _) = select_checkpoint_capped_conversations_lazily(
        vec![conversation(1), conversation(2)],
        None,
        caps,
        &mut load,
    )?;
    assert_eq!(first.last_conversation_id, Some(1));
    assert!(first.stopped_before_candidate);
    let error =
        select_checkpoint_capped_conversations_lazily(vec![conversation(2)], None, caps, &mut load)
            .err()
            .expect("resume must surface the actual storage error");
    assert_eq!(error.to_string(), "later unread body failed");
    Ok(())
}

#[test]
fn byte_caps_preserve_utf8_message_fields_and_exact_final_exhaustion() -> Result<()> {
    let mut original = message(42, 0);
    original.content = "🦀".into();
    original.role = MessageRole::Tool;
    original.author = Some("fixture-tool".into());
    original.extra_json = serde_json::json!({"usage": {"tokens": 99}});
    original.snippets.push(crate::model::types::Snippet {
        id: Some(3),
        file_path: Some("src/main.rs".into()),
        start_line: Some(2),
        end_line: Some(4),
        language: Some("rust".into()),
        snippet_text: Some("let x = 1;".into()),
    });
    let expected = serde_json::to_value(&original)?;
    for last in [false, true] {
        let rows = if last {
            vec![conversation(7)]
        } else {
            vec![conversation(7), conversation(8)]
        };
        let (selection, grouped) = select_checkpoint_capped_conversations_lazily(
            rows,
            None,
            SemanticCheckpointCaps {
                max_messages: 0,
                max_bytes: 4,
            },
            |id| {
                assert_eq!(id, 7);
                Ok(vec![original.clone()])
            },
        )?;
        assert_eq!(serde_json::to_value(&grouped[&7][0])?, expected);
        assert_eq!(selection.last_conversation_id, Some(7));
        assert_eq!(selection.stopped_before_candidate, !last);
        assert_eq!(
            selection.conversations[0].external_id.as_deref(),
            Some("conv-7")
        );
        assert_eq!(
            selection.conversations[0].source_id.as_deref(),
            Some("local")
        );
    }
    Ok(())
}

#[test]
fn capped_pages_emit_every_eligible_message_once_in_source_order() -> Result<()> {
    for shape in 0..64_i64 {
        let bodies: HashMap<i64, Vec<Message>> = (1..=6_i64)
            .map(|id| {
                let count = if shape & (1 << (id - 1)) == 0 {
                    0
                } else {
                    (id % 3 + 1) as usize
                };
                (
                    id,
                    (0..count)
                        .map(|n| message(id * 10 + n as i64, (id % 4) as usize))
                        .collect(),
                )
            })
            .collect();
        for cutoff in [None, Some(31)] {
            for max_messages in [0, 1, 3] {
                for max_bytes in [0, 1, 4] {
                    let mut expected = Vec::new();
                    for id in 1..=6 {
                        for message in &bodies[&id] {
                            if cutoff.is_none_or(|cutoff| message.id.is_some_and(|id| id > cutoff))
                            {
                                expected.push(serde_json::to_value(message)?);
                            }
                        }
                    }
                    let mut actual = Vec::new();
                    let mut cursor = 0;
                    let mut pages = 0;
                    while cursor < 6 {
                        let end = (cursor + 2).min(6);
                        let (selection, grouped) = select_checkpoint_capped_conversations_lazily(
                            (cursor + 1..=end).map(conversation).collect(),
                            cutoff,
                            SemanticCheckpointCaps {
                                max_messages,
                                max_bytes,
                            },
                            |id| Ok(bodies[&id].clone()),
                        )?;
                        let next = selection
                            .last_conversation_id
                            .expect("inspected page must advance");
                        assert!(next > cursor && next <= end);
                        for row in selection.conversations {
                            for message in &grouped[&row.conversation_id] {
                                actual.push(serde_json::to_value(message)?);
                            }
                        }
                        cursor = next;
                        pages += 1;
                        assert!(pages <= 6);
                    }
                    assert_eq!(
                        actual, expected,
                        "shape={shape}, cutoff={cutoff:?}, caps={max_messages}/{max_bytes}"
                    );
                }
            }
        }
    }
    Ok(())
}

#[test]
fn storage_replay_crosses_stale_empty_candidate_pages_without_losing_live_inputs() -> Result<()> {
    use crate::model::types::{Agent, AgentKind};
    let temp = tempfile::tempdir()?;
    let storage = FrankenStorage::open(&temp.path().join("archive.db"))?;
    let agent = storage.ensure_agent(&Agent {
        id: None,
        slug: "codex".into(),
        name: "Codex".into(),
        version: None,
        kind: AgentKind::Cli,
    })?;
    // Stale non-NULL tail hints deliberately nominate four empty parents.
    // All SQL literals are synthetic and this is an isolated temporary archive.
    storage.raw().execute_batch(&format!(
        "BEGIN;
         INSERT INTO conversations(id, agent_id, source_id, external_id, source_path, last_message_idx) VALUES
         (1,{agent},'local','empty-1','/fixture/1.jsonl',0),
         (2,{agent},'local','empty-2','/fixture/2.jsonl',0),
         (3,{agent},'local','empty-3','/fixture/3.jsonl',0),
         (4,{agent},'local','empty-4','/fixture/4.jsonl',0),
         (5,{agent},'local','live-5','/fixture/5.jsonl',1);
         INSERT INTO messages(id, conversation_id, idx, role, content, created_at) VALUES
         (51,5,0,'user','retained compiler evidence',1700000000000),
         (52,5,1,'assistant','retained response evidence',1700000000001);
         COMMIT;"
    ))?;
    let caps = SemanticCheckpointCaps {
        max_messages: 1,
        max_bytes: 1,
    };
    let first = fetch_canonical_embedding_batch_inner_with_caps_and_total(
        &storage, 0, 2, None, caps, 5, None,
    )?;
    assert!(first.inputs.is_empty());
    assert_eq!(first.conversations_in_batch, 0);
    assert_eq!(first.last_conversation_id, 2);
    assert!(!first.cursor_exhausted);
    let second = fetch_canonical_embedding_batch_inner_with_caps_and_total(
        &storage,
        first.last_conversation_id,
        2,
        None,
        caps,
        5,
        None,
    )?;
    assert!(second.inputs.is_empty());
    assert_eq!(second.last_conversation_id, 4);
    assert!(!second.cursor_exhausted);
    let third = fetch_canonical_embedding_batch_inner_with_caps_and_total(
        &storage,
        second.last_conversation_id,
        2,
        None,
        caps,
        5,
        None,
    )?;
    assert_eq!(third.last_conversation_id, 5);
    assert!(third.cursor_exhausted);
    assert_eq!(third.conversations_in_batch, 1);
    assert_eq!(
        third
            .inputs
            .iter()
            .map(|input| input.message_id)
            .collect::<Vec<_>>(),
        [51, 52]
    );
    let mut visited = Vec::new();
    visit_packet_embedding_inputs_from_storage_with_limits(&storage, 2, caps, |input| {
        visited.push((input.message_id, input.content));
        Ok(())
    })?;
    assert_eq!(
        visited,
        vec![
            (51, "retained compiler evidence".into()),
            (52, "retained response evidence".into())
        ]
    );
    let remaining: Vec<(i64, String)> = storage.raw().query_map_collect(
        "SELECT id, content FROM messages ORDER BY id",
        &[] as &[ParamValue],
        |row| Ok((row.get_typed(0)?, row.get_typed(1)?)),
    )?;
    assert_eq!(
        remaining,
        vec![
            (51, "retained compiler evidence".into()),
            (52, "retained response evidence".into())
        ]
    );
    Ok(())
}
