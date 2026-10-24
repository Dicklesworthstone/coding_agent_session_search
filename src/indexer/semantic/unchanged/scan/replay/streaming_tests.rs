//! Whole-packet equivalence and production replay through real canonical storage.
use super::*;
use crate::model::types::{Agent, AgentKind, MessageRole};
use std::cell::Cell;
use std::collections::HashSet;
use std::io;
use std::path::Path;

fn fixture(data: &Path) -> Result<FrankenStorage> {
    let storage = FrankenStorage::open(&data.join("agent_search.db"))?;
    let agent = storage.ensure_agent(&Agent {
        id: None,
        slug: "codex".into(),
        name: "Codex".into(),
        version: None,
        kind: AgentKind::Cli,
    })?;
    let workspace = storage.ensure_workspace(Path::new("/streamed-proof/workspace"), None)?;
    for ordinal in 0..3 {
        let roles = [
            MessageRole::User,
            MessageRole::Agent,
            MessageRole::Tool,
            MessageRole::System,
            MessageRole::Other("custom".into()),
        ];
        let messages = (0..16)
            .map(|idx| Message {
                id: None,
                idx,
                role: roles[idx as usize % roles.len()].clone(),
                author: (idx % 2 == 0).then(|| "fixture".into()),
                created_at: (idx % 3 != 0).then_some(1_700_000_000_000 + idx),
                content: if ordinal == 1 || idx == 7 {
                    " \n\t ".into()
                } else if idx == 2 {
                    "Unicode cafe\u{301} 東京 compiler ownership. ".repeat(800)
                } else {
                    format!("canonical replay parent {ordinal} message {idx}")
                },
                extra_json: serde_json::json!({"not_semantic": "retained by canonical storage"}),
                snippets: Vec::new(),
            })
            .collect();
        storage.insert_conversation_tree(
            agent,
            Some(workspace),
            &Conversation {
                id: None,
                agent_slug: "codex".into(),
                workspace: Some("/streamed-proof/workspace".into()),
                external_id: Some(format!("stream-{ordinal}")),
                title: Some("streamed proof".into()),
                source_path: format!("/streamed-proof/{ordinal}.jsonl").into(),
                started_at: Some(10),
                ended_at: Some(20),
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

fn signature(inputs: Vec<engine::EmbeddingInput>) -> Vec<(Option<String>, String)> {
    inputs
        .into_iter()
        .map(|input| (engine::semantic_doc_id_for_input(&input), input.content))
        .collect()
}

#[test]
fn streamed_single_message_packets_match_full_canonical_replay_and_coverage() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = fixture(temp.path())?;
    storage
        .raw()
        .execute("UPDATE conversations SET origin_host = 'FixtureHost' WHERE id = 1")?;
    storage
        .raw()
        .execute("UPDATE messages SET role = 'assistant' WHERE conversation_id = 1 AND idx = 1")?;
    storage
        .raw()
        .execute("DELETE FROM messages WHERE conversation_id = 3")?;
    storage.raw().execute("PRAGMA query_only = ON")?;
    let incumbent = signature(engine::packet_embedding_inputs_from_storage(&storage)?);
    let mut actual = Vec::new();
    let coverage = visit(
        &storage,
        |input| {
            actual.push(input);
            Ok(())
        },
        || Ok(()),
    )?;
    assert_eq!(
        coverage,
        (2, 2),
        "blank history counts; the deleted parent's stale tail does not"
    );
    assert!(
        actual.len() > 16,
        "the long message must exercise passage splitting"
    );
    assert_eq!(signature(actual), incumbent);
    // Compare the direct row projection with the incumbent's uncapped loader too.
    let original = messages(&storage, 1)?;
    let mut streamed = Vec::new();
    stream::messages(storage.raw(), 1, &mut |message| {
        streamed.push(message);
        Ok(())
    })?;
    assert_eq!(
        serde_json::to_value(streamed)?,
        serde_json::to_value(original)?
    );
    storage.close_without_checkpoint()?;
    Ok(())
}

#[test]
fn cancellation_after_an_input_unwinds_the_native_cursor_and_keeps_connection_usable() -> Result<()>
{
    let temp = tempfile::tempdir()?;
    let storage = fixture(temp.path())?;
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
                Err(io::Error::new(io::ErrorKind::Interrupted, "stop this proof").into())
            } else {
                Ok(())
            }
        },
    )
    .expect_err("a partial parent cannot yield a coverage receipt");
    assert_eq!(delivered, 1);
    assert_eq!(
        error.downcast_ref::<io::Error>().unwrap().kind(),
        io::ErrorKind::Interrupted
    );
    let mut delivered_after = 0;
    assert_eq!(
        visit(
            &storage,
            |_| {
                delivered_after += 1;
                Ok(())
            },
            || Ok(())
        )?,
        (3, 3)
    );
    assert!(delivered_after > delivered);
    storage.close()?;
    Ok(())
}

#[test]
fn typed_delta_survives_native_cursor_abort_and_complete_replay_alone_yields_coverage() -> Result<()>
{
    let temp = tempfile::tempdir()?;
    let storage = fixture(temp.path())?;
    let sink = crate::indexer::semantic_progress::SemanticProgressSink::disabled();
    let mut absent = HashSet::new();
    let (comparison, coverage) =
        super::super::compare_with_coverage(&storage, &mut absent, &sink, 0, || Ok(()))?;
    assert!(!comparison.matches);
    assert_eq!(comparison.visited, 1);
    assert_eq!(
        coverage, None,
        "the SQL abort must not become a partial success"
    );
    let ids: HashSet<_> = engine::packet_embedding_inputs_from_storage(&storage)?
        .iter()
        .filter_map(engine::semantic_doc_id_for_input)
        .collect();
    let mut remaining = ids.iter().map(String::as_str).collect();
    let (comparison, coverage) = super::super::compare_with_coverage(
        &storage,
        &mut remaining,
        &sink,
        ids.len() as u64,
        || Ok(()),
    )?;
    assert!(comparison.matches && remaining.is_empty());
    assert_eq!(coverage, Some((3, 3)));
    storage.close()?;
    Ok(())
}
