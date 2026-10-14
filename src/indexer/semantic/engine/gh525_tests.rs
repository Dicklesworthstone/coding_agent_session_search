use super::*;
use crate::model::types::MessageRole;

fn conversation(id: i64) -> CanonicalEmbeddingConversationRow {
    CanonicalEmbeddingConversationRow {
        conversation_id: id,
        agent_slug: "codex".to_owned(),
        agent_id: 1,
        workspace: None,
        workspace_id: None,
        external_id: Some(format!("conv-{id}")),
        title: None,
        source_path: PathBuf::from(format!("/source/{id}.jsonl")),
        started_at: None,
        ended_at: None,
        source_id: Some("local".to_owned()),
        origin_host: None,
    }
}

fn message(id: i64, bytes: usize) -> Message {
    Message {
        id: Some(id),
        idx: 0,
        role: MessageRole::User,
        author: None,
        created_at: Some(1_700_000_000_000 + id),
        content: "x".repeat(bytes),
        extra_json: serde_json::Value::Null,
        snippets: Vec::new(),
    }
}

#[test]
fn gh525_filled_message_cap_stops_before_loading_the_next_conversation() -> Result<()> {
    let mut loaded = Vec::new();
    let (selection, grouped) = select_checkpoint_capped_conversations_lazily(
        vec![conversation(1), conversation(2), conversation(3)],
        None,
        SemanticCheckpointCaps { max_messages: 1, max_bytes: 0 },
        |id| {
            loaded.push(id);
            Ok(vec![message(id, 4)])
        },
    )?;

    assert_eq!(loaded, vec![1]);
    assert_eq!(selection.conversations.iter().map(|row| row.conversation_id).collect::<Vec<_>>(), vec![1]);
    assert_eq!(selection.last_conversation_id, Some(1));
    assert!(selection.stopped_before_candidate);
    assert_eq!(grouped.keys().copied().collect::<Vec<_>>(), vec![1]);
    Ok(())
}

#[test]
fn gh525_filled_byte_cap_stops_before_loading_the_next_conversation() -> Result<()> {
    let mut loaded = Vec::new();
    let (selection, _) = select_checkpoint_capped_conversations_lazily(
        vec![conversation(1), conversation(2), conversation(3)],
        None,
        SemanticCheckpointCaps { max_messages: 0, max_bytes: 4 },
        |id| {
            loaded.push(id);
            Ok(vec![message(id, 4)])
        },
    )?;

    assert_eq!(loaded, vec![1]);
    assert_eq!(selection.last_conversation_id, Some(1));
    assert!(selection.stopped_before_candidate);
    Ok(())
}

#[test]
fn gh525_first_oversized_conversation_still_advances_whole() -> Result<()> {
    let mut loaded = Vec::new();
    let (selection, grouped) = select_checkpoint_capped_conversations_lazily(
        vec![conversation(1), conversation(2)],
        None,
        SemanticCheckpointCaps { max_messages: 1, max_bytes: 1 },
        |id| {
            loaded.push(id);
            Ok(vec![message(id, 8), message(id + 100, 8)])
        },
    )?;

    assert_eq!(loaded, vec![1]);
    assert_eq!(selection.conversations.len(), 1);
    assert_eq!(selection.last_conversation_id, Some(1));
    assert!(selection.stopped_before_candidate);
    assert_eq!(grouped.get(&1).map(Vec::len), Some(2));
    Ok(())
}

#[test]
fn gh525_loads_one_more_candidate_only_when_remaining_capacity_requires_its_size() -> Result<()> {
    let mut loaded = Vec::new();
    let (selection, grouped) = select_checkpoint_capped_conversations_lazily(
        vec![conversation(1), conversation(2), conversation(3)],
        None,
        SemanticCheckpointCaps { max_messages: 3, max_bytes: 0 },
        |id| {
            loaded.push(id);
            Ok(vec![message(id * 10, 2), message(id * 10 + 1, 2)])
        },
    )?;

    assert_eq!(loaded, vec![1, 2]);
    assert_eq!(selection.conversations.iter().map(|row| row.conversation_id).collect::<Vec<_>>(), vec![1]);
    assert!(selection.stopped_before_candidate);
    assert!(grouped.contains_key(&1));
    assert!(!grouped.contains_key(&2));
    Ok(())
}

#[test]
fn gh525_message_cursor_filtering_does_not_advance_on_an_empty_candidate() -> Result<()> {
    let mut loaded = Vec::new();
    let (selection, grouped) = select_checkpoint_capped_conversations_lazily(
        vec![conversation(1), conversation(2)],
        Some(10),
        SemanticCheckpointCaps { max_messages: 1, max_bytes: 0 },
        |id| {
            loaded.push(id);
            Ok(vec![message(if id == 1 { 10 } else { 11 }, 4)])
        },
    )?;

    assert_eq!(loaded, vec![1, 2]);
    assert_eq!(selection.last_conversation_id, Some(2));
    assert!(!selection.stopped_before_candidate);
    assert!(!grouped.contains_key(&1));
    assert!(grouped.contains_key(&2));
    Ok(())
}

mod real_storage;
mod continuation;
