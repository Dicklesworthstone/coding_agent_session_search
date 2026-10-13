//! GH525: use real canonical rows, and observe admission at the real body loader.
use super::super::{
    CanonicalEmbeddingConversationRow, CheckpointCappedSelection, Message, SemanticCheckpointCaps,
    select_checkpoint_capped_conversations_lazily,
    EmbeddingInput, fetch_canonical_embedding_batch_inner_with_caps_and_total,
    fetch_canonical_embedding_conversations, packet_embedding_inputs_from_materialized_canonical_messages,
    semantic_doc_id_for_input,
};
use crate::franken_sync::{FrankenError, compat::{ConnectionExt, ParamValue, RowExt}};
use crate::model::types::{Agent, AgentKind, Conversation, MessageRole};
use crate::storage::sqlite::FrankenStorage;
use std::path::Path;
use std::collections::HashMap;
use anyhow::Result;

fn fixture(path: &Path, histories: &[&[&str]]) -> Result<(FrankenStorage, Vec<i64>)> {
    let storage = FrankenStorage::open(path)?;
    let agent = storage.ensure_agent(&Agent {
        id: None, slug: "codex".into(), name: "Codex".into(), version: None, kind: AgentKind::Cli,
    })?;
    for (ordinal, history) in histories.iter().enumerate() {
        let messages = history.iter().enumerate().map(|(idx, text)| Message {
            id: None, idx: idx as i64, role: MessageRole::User, author: None,
            created_at: Some(1_700_000_000_000 + idx as i64), content: (*text).into(),
            extra_json: serde_json::Value::Null, snippets: Vec::new(),
        }).collect();
        storage.insert_conversation_tree(agent, None, &Conversation {
            id: None, agent_slug: "codex".into(), workspace: None,
            external_id: Some(format!("gh525-{ordinal}")), title: None,
            source_path: format!("/gh525/{ordinal}.jsonl").into(),
            started_at: Some(1), ended_at: Some(2), approx_tokens: None,
            metadata_json: serde_json::Value::Null, messages,
            source_id: "local".into(), origin_host: None,
        })?;
    }
    let ids = storage.raw().query_map_collect(
        "SELECT id FROM conversations ORDER BY id", &[] as &[ParamValue], |row| row.get_typed(0),
    )?;
    Ok((storage, ids))
}

fn body(storage: &FrankenStorage, id: i64) -> Result<Vec<Message>> {
    let mut rows = storage.fetch_messages_for_lexical_rebuild_batch(&[id], None, None)?;
    Ok(rows.remove(&id).unwrap_or_default())
}

// Adapt the production helper's tuple only for readable assertions. All body
// admission and cap decisions below execute its actual implementation.
struct Selection {
    conversations: Vec<CanonicalEmbeddingConversationRow>,
    grouped_messages: HashMap<i64, Vec<Message>>,
    last_conversation_id: Option<i64>,
    stopped_before_candidate: bool,
}

fn load(
    conversations: Vec<CanonicalEmbeddingConversationRow>,
    after: Option<i64>,
    caps: SemanticCheckpointCaps,
    loader: impl FnMut(i64) -> Result<Vec<Message>>,
) -> Result<Selection> {
    let (selection, grouped_messages) =
        select_checkpoint_capped_conversations_lazily(conversations, after, caps, loader)?;
    Ok(Selection {
        conversations: selection.conversations,
        grouped_messages,
        last_conversation_id: selection.last_conversation_id,
        stopped_before_candidate: selection.stopped_before_candidate,
    })
}

fn measured(
    storage: &FrankenStorage, ids: &[i64], after: Option<i64>, caps: SemanticCheckpointCaps,
    loads: &mut Vec<i64>,
) -> Result<Selection> {
    load(fetch_canonical_embedding_conversations(storage, ids)?, after, caps, |id| {
        loads.push(id);
        body(storage, id)
    })
}

fn signature(inputs: Vec<EmbeddingInput>) -> Vec<(Option<String>, String)> {
    inputs.into_iter().map(|input| (semantic_doc_id_for_input(&input), input.content)).collect()
}

#[test]
fn gh525_filled_message_or_byte_cap_admits_only_the_first_body() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (storage, ids) = fixture(&temp.path().join("cap.db"), &[&["first"], &["second"], &["third"]])?;
    for caps in [
        SemanticCheckpointCaps { max_messages: 1, max_bytes: 0 },
        SemanticCheckpointCaps { max_messages: 0, max_bytes: 5 },
        SemanticCheckpointCaps { max_messages: 1, max_bytes: 5 },
    ] {
        let mut loads = Vec::new();
        let selected = measured(&storage, &ids, None, caps, &mut loads)?;
        assert_eq!(loads, ids[..1], "no body admission after a filled cap");
        assert_eq!(selected.conversations.len(), 1);
        assert_eq!(selected.grouped_messages.len(), 1);
        assert_eq!(selected.grouped_messages[&ids[0]][0].content, "first");
        assert_eq!(selected.last_conversation_id, Some(ids[0]));
        assert!(selected.stopped_before_candidate);
    }
    storage.close()?;
    Ok(())
}

#[test]
fn gh525_first_oversized_history_is_kept_whole_and_does_not_admit_a_successor() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let long = "Unicode 東京 cafe\u{301} full canonical history ".repeat(1024);
    let (storage, ids) = fixture(&temp.path().join("large.db"), &[&[&long, "tail"], &["later"]])?;
    let caps = SemanticCheckpointCaps { max_messages: 1, max_bytes: 1 };
    let mut loads = Vec::new();
    let selected = measured(&storage, &ids, None, caps, &mut loads)?;
    assert_eq!(loads, ids[..1]);
    assert_eq!(selected.grouped_messages[&ids[0]].len(), 2);
    assert_eq!(selected.grouped_messages[&ids[0]][0].content, long);
    assert_eq!(selected.grouped_messages[&ids[0]][1].content, "tail");
    assert_eq!(selected.last_conversation_id, Some(ids[0]));
    assert!(selected.stopped_before_candidate);
    let mut loads = Vec::new();
    let final_page = measured(&storage, &ids[1..], None, caps, &mut loads)?;
    assert_eq!(loads, ids[1..]);
    assert_eq!(final_page.last_conversation_id, Some(ids[1]));
    assert!(!final_page.stopped_before_candidate, "the final oversized candidate exhausts its page");
    storage.close()?;
    Ok(())
}

#[test]
fn gh525_partial_budget_loads_at_most_one_rejected_candidate_and_keeps_its_cursor_pending() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (storage, ids) = fixture(&temp.path().join("partial.db"), &[&["a", "b"], &["c", "d"], &["e"]])?;
    for caps in [
        SemanticCheckpointCaps { max_messages: 3, max_bytes: 0 },
        SemanticCheckpointCaps { max_messages: 0, max_bytes: 3 },
    ] {
        let mut loads = Vec::new();
        let selected = measured(&storage, &ids, None, caps, &mut loads)?;
        assert_eq!(loads, ids[..2]);
        assert_eq!(selected.last_conversation_id, Some(ids[0]));
        assert!(selected.stopped_before_candidate);
        assert!(!selected.grouped_messages.contains_key(&ids[1]));
        assert!(!selected.grouped_messages.contains_key(&ids[2]));
        let mut loads = Vec::new();
        let next = measured(&storage, &ids[1..], None, caps, &mut loads)?;
        assert_eq!(next.grouped_messages[&ids[1]].len(), 2);
        assert_eq!(next.grouped_messages[&ids[2]].len(), 1);
        assert!(!next.stopped_before_candidate);
    }
    storage.close()?;
    Ok(())
}

#[test]
fn gh525_message_cursor_filters_before_accounting_and_empty_histories_do_not_spend_caps() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (storage, ids) = fixture(&temp.path().join("cursor.db"), &[&[], &["old", "new"], &["later"]])?;
    let old_id = body(&storage, ids[1])?[0].id.unwrap();
    let mut loads = Vec::new();
    let selected = measured(&storage, &ids, Some(old_id), SemanticCheckpointCaps {
        max_messages: 1, max_bytes: 3,
    }, &mut loads)?;
    assert_eq!(loads, ids[..2]);
    assert_eq!(selected.conversations.len(), 1);
    assert_eq!(selected.grouped_messages[&ids[1]].len(), 1);
    assert_eq!(selected.grouped_messages[&ids[1]][0].content, "new");
    assert!(selected.grouped_messages[&ids[1]][0].id.unwrap() > old_id);
    assert_eq!(selected.last_conversation_id, Some(ids[1]));
    assert!(selected.stopped_before_candidate);
    let mut loads = Vec::new();
    let empty = measured(&storage, &ids, Some(i64::MAX), SemanticCheckpointCaps {
        max_messages: 1, max_bytes: 1,
    }, &mut loads)?;
    assert_eq!(loads, ids);
    assert!(empty.conversations.is_empty() && empty.grouped_messages.is_empty());
    assert_eq!(empty.last_conversation_id, None);
    assert!(!empty.stopped_before_candidate);
    storage.close()?;
    Ok(())
}

#[test]
fn gh525_unlimited_and_partially_filled_selection_match_the_incumbent_encoder() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (storage, ids) = fixture(&temp.path().join("oracle.db"), &[&[], &["alpha", "東京"], &["beta"], &["gamma", "delta"]])?;
    for caps in [
        SemanticCheckpointCaps::unlimited(),
        SemanticCheckpointCaps { max_messages: 4, max_bytes: 0 },
        SemanticCheckpointCaps { max_messages: 0, max_bytes: 12 },
    ] {
        let mut eager = storage.fetch_messages_for_lexical_rebuild_batch(&ids, None, None)?;
        let old = select_checkpoint_capped_conversations(
            fetch_canonical_embedding_conversations(&storage, &ids)?, &mut eager, None, caps,
        );
        let mut loads = Vec::new();
        let mut new = measured(&storage, &ids, None, caps, &mut loads)?;
        assert_eq!(new.last_conversation_id, old.last_conversation_id);
        assert_eq!(new.stopped_before_candidate, old.stopped_before_candidate);
        assert_eq!(new.conversations.len(), old.conversations.len());
        let (expected, _) = packet_embedding_inputs_from_materialized_canonical_messages(
            &old.conversations, &mut eager, |_| true,
        );
        let (actual, _) = packet_embedding_inputs_from_materialized_canonical_messages(
            &new.conversations, &mut new.grouped_messages, |_| true,
        );
        assert_eq!(signature(actual), signature(expected));
    }
    storage.close()?;
    Ok(())
}

#[test]
fn gh525_loader_failure_is_typed_and_cannot_return_partial_success() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (storage, ids) = fixture(&temp.path().join("failure.db"), &[&["first"], &["second"], &["third"]])?;
    let mut loads = Vec::new();
    let result = load(fetch_canonical_embedding_conversations(&storage, &ids)?, None,
        SemanticCheckpointCaps::unlimited(), |id| {
            loads.push(id);
            if id == ids[1] {
                storage.raw().query("SELECT * FROM gh525_missing_table")?;
            }
            body(&storage, id)
        });
    let error = result.err().expect("the SQL failure cannot yield a partial checkpoint");
    assert!(error.downcast_ref::<FrankenError>().is_some(), "{error:#}");
    assert_eq!(loads, ids[..2]);
    assert_eq!(body(&storage, ids[0])?[0].content, "first");
    storage.close()?;
    Ok(())
}

#[test]
fn gh525_real_batch_path_defers_unread_later_body_errors_until_resume() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (storage, ids) = fixture(&temp.path().join("deferred.db"), &[&["first"], &["later"]])?;
    storage.raw().execute_compat(
        "UPDATE messages SET role = X'00' WHERE conversation_id = ?1", &[ParamValue::from(ids[1])],
    )?;
    assert!(body(&storage, ids[1]).is_err(), "the later body must really be undecodable");
    let caps = SemanticCheckpointCaps { max_messages: 1, max_bytes: 0 };
    let batch = fetch_canonical_embedding_batch_inner_with_caps_and_total(
        &storage, 0, 8, None, caps, 2, None,
    )?;
    assert_eq!(batch.conversations_in_batch, 1);
    assert_eq!(batch.last_conversation_id, ids[0]);
    assert!(!batch.cursor_exhausted);
    assert_eq!(batch.inputs[0].content, "first");
    let failure = fetch_canonical_embedding_batch_inner_with_caps_and_total(
        &storage, batch.last_conversation_id, 8, None, caps, 2, None,
    ).err().expect("resume must observe, not skip, the deferred read failure");
    assert!(failure.downcast_ref::<FrankenError>().is_some(), "{failure:#}");
    storage.close()?;
    Ok(())
}

#[test]
fn gh525_successive_production_batches_preserve_full_coverage_and_exact_exhaustion() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (storage, ids) = fixture(&temp.path().join("pages.db"), &[&["first"], &[""], &["third"], &["last"]])?;
    let all = fetch_canonical_embedding_batch_inner_with_caps_and_total(
        &storage, 0, 8, None, SemanticCheckpointCaps::unlimited(), 4, None,
    )?;
    let expected = signature(all.inputs);
    for caps in [
        SemanticCheckpointCaps { max_messages: 1, max_bytes: 0 },
        SemanticCheckpointCaps { max_messages: 0, max_bytes: 5 },
    ] {
        let mut after = 0;
        let mut actual = Vec::new();
        let mut conversations = 0;
        let mut completed = false;
        for _ in 0..=ids.len() {
            let batch = fetch_canonical_embedding_batch_inner_with_caps_and_total(
                &storage, after, 8, None, caps, 4, None,
            )?;
            assert_eq!(batch.total_conversations, 4);
            assert!(batch.last_conversation_id > after);
            after = batch.last_conversation_id;
            conversations += batch.conversations_in_batch;
            actual.extend(batch.inputs);
            if batch.cursor_exhausted { completed = true; break; }
        }
        assert!(completed);
        assert_eq!(conversations, 4, "blank bodies still consume canonical conversation coverage");
        assert_eq!(after, *ids.last().unwrap());
        assert_eq!(signature(actual), expected);
    }
    storage.close()?;
    Ok(())
}

// Pre-change eager admission is a test-only oracle; it is not used by CASS.
fn select_checkpoint_capped_conversations(
    conversations: Vec<CanonicalEmbeddingConversationRow>,
    grouped_messages: &mut HashMap<i64, Vec<Message>>,
    after_message_id: Option<i64>,
    caps: SemanticCheckpointCaps,
) -> CheckpointCappedSelection {
    let mut selected = Vec::new();
    let mut selected_messages = 0usize;
    let mut selected_bytes = 0u64;
    let mut last_conversation_id = None;
    let mut stopped_before_candidate = false;

    for conversation in conversations {
        let mut messages = grouped_messages
            .remove(&conversation.conversation_id)
            .unwrap_or_default();
        if let Some(min_exclusive) = after_message_id {
            messages.retain(|message| message.id.is_some_and(|id| id > min_exclusive));
        }
        if messages.is_empty() {
            continue;
        }

        let message_count = messages.len();
        let byte_count = messages
            .iter()
            .map(|message| super::super::saturating_u64_from_usize(message.content.len()))
            .fold(0u64, u64::saturating_add);
        let would_exceed_messages = caps.message_limited()
            && !selected.is_empty()
            && selected_messages.saturating_add(message_count) > caps.max_messages;
        let would_exceed_bytes = caps.byte_limited()
            && !selected.is_empty()
            && selected_bytes.saturating_add(byte_count) > caps.max_bytes;
        if would_exceed_messages || would_exceed_bytes {
            stopped_before_candidate = true;
            tracing::debug!(
                conversation_id = conversation.conversation_id,
                selected_conversations = selected.len(),
                selected_messages,
                selected_bytes,
                candidate_messages = message_count,
                candidate_bytes = byte_count,
                max_messages_per_checkpoint = caps.max_messages,
                max_bytes_per_checkpoint = caps.max_bytes,
                "semantic checkpoint cap stopped this batch before the next full conversation"
            );
            break;
        }

        selected_messages = selected_messages.saturating_add(message_count);
        selected_bytes = selected_bytes.saturating_add(byte_count);
        last_conversation_id = Some(conversation.conversation_id);
        grouped_messages.insert(conversation.conversation_id, messages);
        selected.push(conversation);
    }

    CheckpointCappedSelection {
        conversations: selected,
        last_conversation_id,
        stopped_before_candidate,
    }
}

#[test]
fn gh525_checkpoint_publication_waits_for_complete_resumed_canonical_coverage() -> Result<()> {
    use super::super::{SemanticBackfillBatchPlan, SemanticIndexer};
    use crate::search::semantic_manifest::{SemanticManifest, TierKind};
    use std::collections::HashSet;

    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let (storage, ids) = fixture(&data.join("checkpoint.db"), &[&["first"], &[""], &["third"], &["last"]])?;
    let complete = fetch_canonical_embedding_batch_inner_with_caps_and_total(
        &storage, 0, 8, None, SemanticCheckpointCaps::unlimited(), 4, None,
    )?;
    let expected: HashSet<_> = complete.inputs.iter().filter_map(semantic_doc_id_for_input).collect();
    let indexer = SemanticIndexer::new("hash", None)?;
    let mut manifest = SemanticManifest::default();
    let mut after = 0;
    let mut published = None;
    for _ in 0..=ids.len() {
        let batch = fetch_canonical_embedding_batch_inner_with_caps_and_total(
            &storage, after, 8, None,
            SemanticCheckpointCaps { max_messages: 1, max_bytes: 0 }, 4, None,
        )?;
        assert!(batch.last_conversation_id > after);
        after = batch.last_conversation_id;
        let outcome = indexer.run_backfill_batch(
            &batch.inputs, data, &mut manifest,
            SemanticBackfillBatchPlan {
                tier: TierKind::Quality, db_fingerprint: "gh525:canonical".into(),
                model_revision: "hash".into(), total_conversations: 4,
                conversations_in_batch: batch.conversations_in_batch,
                last_offset: after, cursor_exhausted: batch.cursor_exhausted,
            },
        )?;
        assert_eq!(outcome.published, batch.cursor_exhausted);
        if batch.cursor_exhausted {
            assert!(manifest.checkpoint.is_none());
            published = Some(outcome.index_path);
            break;
        }
        assert!(outcome.checkpoint_saved);
        assert!(!manifest.quality_tier.as_ref().is_some_and(|tier| tier.ready));
        assert_eq!(manifest.checkpoint.as_ref().unwrap().last_offset, after);
        // Resume with the actual saved ledger rather than only in-memory state.
        manifest = SemanticManifest::load(data)?.expect("saved checkpoint ledger");
    }
    assert_eq!(after, *ids.last().unwrap());
    let published = published.expect("the complete traversal must publish");
    let reopened = frankensearch::index::VectorIndex::open_read_only(&published)?;
    let actual: HashSet<_> = (0..reopened.record_count())
        .map(|row| reopened.doc_id_at(row).map(str::to_owned))
        .collect::<std::result::Result<_, _>>()?;
    assert_eq!(actual, expected);
    assert_eq!(reopened.record_count(), expected.len());
    assert_eq!(manifest.quality_tier.as_ref().unwrap().conversation_count, 4);
    storage.close()?;
    Ok(())
}
