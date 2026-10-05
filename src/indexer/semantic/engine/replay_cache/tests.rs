//! Real canonical-store and hash/FSVI coverage. No model downloads or verdict mocks.

use super::*;
use crate::model::types::{Agent, AgentKind, MessageRole};
use serde_json::json;
use std::time::Instant;

fn fixture(bodies: &[&str]) -> Result<(tempfile::TempDir, FrankenStorage)> {
    let dir = tempfile::tempdir()?;
    let storage = FrankenStorage::open(&dir.path().join("agent_search.db"))?;
    let agent = storage.ensure_agent(&Agent {
        id: None, slug: "codex".into(), name: "Codex".into(),
        version: None, kind: AgentKind::Cli,
    })?;
    for (ordinal, body) in bodies.iter().enumerate() {
        storage.insert_conversation_tree(agent, None, &Conversation {
            id: None, agent_slug: "codex".into(), workspace: None,
            external_id: Some(format!("replay-{ordinal}")),
            title: Some(format!("replay {ordinal}")),
            source_path: dir.path().join(format!("source-{ordinal}.jsonl")),
            started_at: Some(1_700_000_000_000), ended_at: Some(1_700_000_001_000),
            approx_tokens: None, metadata_json: json!({}),
            source_id: "local".into(), origin_host: None,
            messages: vec![Message {
                id: None, idx: 0, role: MessageRole::User, author: None,
                created_at: Some(1_700_000_000_500), content: (*body).into(),
                extra_json: json!({}), snippets: Vec::new(),
            }],
        })?;
    }
    Ok((dir, storage))
}

fn plan(limit: usize) -> SemanticBackfillStoragePlan {
    SemanticBackfillStoragePlan {
        tier: TierKind::Fast, db_fingerprint: "replay-cache-test".into(),
        model_revision: "hash".into(), max_conversations: limit,
    }
}

fn cache(bytes: usize) -> CanonicalReplayCache {
    CanonicalReplayCache { slot: Mutex::new(None), max_bytes: bytes }
}

fn signature(selection: &ReconcileSelection) -> Vec<(u64, u8, String)> {
    selection.inputs.iter().map(|input| {
        (input.message_id, input.chunk_idx, semantic_doc_id_for_input(input).unwrap())
    }).collect()
}

fn select(
    cache: &CanonicalReplayCache,
    indexer: &SemanticIndexer,
    storage: &FrankenStorage,
    existing: &HashSet<String>,
    plan: &SemanticBackfillStoragePlan,
    caps: SemanticCheckpointCaps,
) -> Result<(ReconcileSelection, ReplayProof)> {
    cache.select(indexer, storage, plan, existing, caps, &SemanticProgressSink::disabled())
}

#[test]
#[cfg(unix)]
fn replay_cache_reads_the_archive_once_and_only_reloads_selected_text() -> Result<()> {
    let (_dir, storage) = fixture(&["first message", "second message", "third message", "last message"])?;
    assert!(BackfillFilePair::archive(&storage).is_some(), "fixture needs a descriptor-bound observation");
    let indexer = SemanticIndexer::new("hash", None)?;
    let cache = cache(MAX_RETAINED_BYTES);
    let baseline = cache_disabled();
    let plan = plan(1);
    let mut existing = HashSet::new();
    let mut canonical = None;
    for round in 0..4 {
        let (expected, _) = select(&baseline, &indexer, &storage, &existing, &plan, SemanticCheckpointCaps::unlimited())?;
        let (actual, proof) = select(&cache, &indexer, &storage, &existing, &plan, SemanticCheckpointCaps::unlimited())?;
        assert_eq!(signature(&actual), signature(&expected));
        assert_eq!(actual.current_ids, expected.current_ids);
        assert_eq!(actual.covered_conversations, expected.covered_conversations);
        assert_eq!(actual.last_covered_conversation, expected.last_covered_conversation);
        assert_eq!(actual.last_message_id, expected.last_message_id);
        assert_eq!(actual.replayed_conversations, if round == 0 { 4 } else { 0 });
        assert_eq!(actual.loaded_selected_conversations, usize::from(round != 0));
        assert_eq!(actual.inputs[0].message_id, round + 1);
        proof.verify(&storage)?;
        canonical.get_or_insert_with(|| actual.current_ids.clone());
        existing.extend(actual.selected_ids);
    }
    assert_eq!(Some(existing), canonical);
    Ok(())
}

fn cache_disabled() -> CanonicalReplayCache { cache(0) }

#[test]
fn replay_cache_budget_refusal_preserves_the_full_uncached_selection() -> Result<()> {
    let (_dir, storage) = fixture(&["first message", "second message", "third message"])?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let tiny = cache(1);
    let disabled = cache_disabled();
    let plan = plan(2);
    for _ in 0..2 {
        let (actual, _) = select(&tiny, &indexer, &storage, &HashSet::new(), &plan, SemanticCheckpointCaps::unlimited())?;
        let (expected, _) = select(&disabled, &indexer, &storage, &HashSet::new(), &plan, SemanticCheckpointCaps::unlimited())?;
        assert_eq!(signature(&actual), signature(&expected));
        assert_eq!(actual.current_ids.len(), 3);
        assert_eq!(actual.inputs.len(), 2);
        assert_eq!(actual.replayed_conversations, 3);
        assert!(tiny.slot.lock().unwrap().is_none());
    }
    Ok(())
}

#[test]
#[cfg(unix)]
fn replay_cache_does_not_confuse_selected_inputs_with_persisted_coverage() -> Result<()> {
    let (_dir, storage) = fixture(&["first message", "second message"])?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let cache = cache(MAX_RETAINED_BYTES);
    let plan = plan(1);
    let empty = HashSet::new();
    let (first, _) = select(&cache, &indexer, &storage, &empty, &plan, SemanticCheckpointCaps::unlimited())?;
    // Simulate retry after a failed embedding/publication by leaving the REAL
    // caller-supplied coverage set unchanged, not by reporting synthetic success.
    let (retry, _) = select(&cache, &indexer, &storage, &empty, &plan, SemanticCheckpointCaps::unlimited())?;
    assert_eq!(signature(&first), signature(&retry));
    assert_eq!(retry.replayed_conversations, 0);
    assert_eq!(retry.inputs[0].message_id, 1);
    assert_eq!(retry.covered_conversations, 1);
    Ok(())
}

#[test]
#[cfg(unix)]
fn replay_cache_same_id_edit_with_unchanged_count_and_tail_invalidates_inventory() -> Result<()> {
    let (_dir, storage) = fixture(&["old content", "second message", "tail message"])?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let cache = cache(MAX_RETAINED_BYTES);
    let plan = plan(3);
    let (before, _) = select(&cache, &indexer, &storage, &HashSet::new(), &plan, SemanticCheckpointCaps::unlimited())?;
    let fingerprint = crate::indexer::lexical_storage_fingerprint_for_storage(&storage)?;
    storage.raw().execute("UPDATE messages SET content = 'new content' WHERE conversation_id = 1")?;
    assert_eq!(crate::indexer::lexical_storage_fingerprint_for_storage(&storage)?, fingerprint);
    let (after, _) = select(&cache, &indexer, &storage, &before.current_ids, &plan, SemanticCheckpointCaps::unlimited())?;
    assert_eq!(after.replayed_conversations, 3);
    assert_eq!(after.inputs.len(), 1);
    assert_eq!(after.inputs[0].content, "new content");
    assert_eq!(after.current_ids.intersection(&before.current_ids).count(), 2);
    assert_eq!(after.current_ids.len(), 3);
    Ok(())
}

#[test]
#[cfg(unix)]
fn replay_cache_deletion_and_changed_provenance_cannot_reuse_old_identities() -> Result<()> {
    let (_dir, storage) = fixture(&["deleted message", "identity message", "retained message"])?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let cache = cache(MAX_RETAINED_BYTES);
    let plan = plan(3);
    let (before, _) = select(&cache, &indexer, &storage, &HashSet::new(), &plan, SemanticCheckpointCaps::unlimited())?;
    storage.raw().execute("DELETE FROM messages WHERE conversation_id = 1")?;
    storage.raw().execute("UPDATE messages SET role = 'assistant' WHERE conversation_id = 2")?;
    let (after, _) = select(&cache, &indexer, &storage, &before.current_ids, &plan, SemanticCheckpointCaps::unlimited())?;
    assert_eq!(after.replayed_conversations, 3);
    assert_eq!(after.current_ids.len(), 2);
    assert_eq!(after.inputs.len(), 1);
    assert_eq!(after.inputs[0].message_id, 2);
    assert_eq!(after.current_ids.intersection(&before.current_ids).count(), 1);
    assert_eq!(after.covered_conversations, 3, "empty parent still counts as covered");
    Ok(())
}

#[test]
#[cfg(unix)]
fn replay_cache_archive_and_producer_contexts_never_alias() -> Result<()> {
    let (_a, a) = fixture(&["first archive"])?;
    let (_b, b) = fixture(&["other archive"])?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let cache = cache(MAX_RETAINED_BYTES);
    let mut plan = plan(1);
    let empty = HashSet::new();
    select(&cache, &indexer, &a, &empty, &plan, SemanticCheckpointCaps::unlimited())?;
    let (other, _) = select(&cache, &indexer, &b, &empty, &plan, SemanticCheckpointCaps::unlimited())?;
    assert_eq!(other.replayed_conversations, 1);
    assert_eq!(other.inputs[0].content, "other archive");
    plan.model_revision = "different-producer-contract".into();
    let (changed, _) = select(&cache, &indexer, &b, &empty, &plan, SemanticCheckpointCaps::unlimited())?;
    assert_eq!(changed.replayed_conversations, 1);
    plan.db_fingerprint = "different-input-contract".into();
    let (changed, _) = select(&cache, &indexer, &b, &empty, &plan, SemanticCheckpointCaps::unlimited())?;
    assert_eq!(changed.replayed_conversations, 1);
    Ok(())
}

#[test]
#[cfg(unix)]
fn replay_cache_hit_cannot_publish_after_a_later_canonical_write() -> Result<()> {
    let (_dir, storage) = fixture(&["first message", "second message"])?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let cache = cache(MAX_RETAINED_BYTES);
    let plan = plan(1);
    select(&cache, &indexer, &storage, &HashSet::new(), &plan, SemanticCheckpointCaps::unlimited())?;
    let (_, proof) = select(&cache, &indexer, &storage, &HashSet::new(), &plan, SemanticCheckpointCaps::unlimited())?;
    assert!(proof.0.is_some(), "negative requires an actual inventory hit");
    proof.verify(&storage)?;
    storage.raw().execute("UPDATE messages SET content = 'changed during embedding' WHERE conversation_id = 2")?;
    assert!(proof.verify(&storage).unwrap_err().to_string().contains("archive changed"));
    Ok(())
}

#[test]
fn replay_cache_preserves_later_small_fit_and_first_oversized_conversation() -> Result<()> {
    let large = "longword ".repeat(20);
    let (_dir, storage) = fixture(&["small one", &large, "small two"])?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let cache = cache(MAX_RETAINED_BYTES);
    let plan = plan(3);
    let caps = SemanticCheckpointCaps { max_messages: 0, max_bytes: 32 };
    for _ in 0..2 {
        let (selection, _) = select(&cache, &indexer, &storage, &HashSet::new(), &plan, caps)?;
        assert_eq!(selection.inputs.iter().map(|input| input.message_id).collect::<Vec<_>>(), vec![1, 3]);
        assert_eq!(selection.covered_conversations, 2);
        let (oversized, _) = select(&cache, &indexer, &storage, &selection.selected_ids, &plan, caps)?;
        assert_eq!(oversized.inputs.iter().map(|input| input.message_id).collect::<Vec<_>>(), vec![2]);
        assert_eq!(oversized.covered_conversations, 3);
    }
    Ok(())
}

#[test]
#[cfg(unix)]
fn replay_cache_is_not_available_inside_an_explicit_read_transaction() -> Result<()> {
    let (_dir, storage) = fixture(&["first message"])?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let cache = cache(MAX_RETAINED_BYTES);
    let plan = plan(1);
    select(&cache, &indexer, &storage, &HashSet::new(), &plan, SemanticCheckpointCaps::unlimited())?;
    storage.raw().as_async().begin_transaction_sync()?;
    let result = select(&cache, &indexer, &storage, &HashSet::new(), &plan, SemanticCheckpointCaps::unlimited());
    storage.raw().as_async().rollback_transaction_sync()?;
    let (selected, proof) = result?;
    assert_eq!(selected.replayed_conversations, 1);
    assert!(proof.0.is_none());
    assert!(cache.slot.lock().unwrap().is_none());
    Ok(())
}

#[test]
#[cfg(unix)]
fn replay_cache_live_backfill_converges_with_identical_published_fsvi_coverage() -> Result<()> {
    let (dir, storage) = fixture(&["first message", "second message", "third message", "last message"])?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let mut manifest = SemanticManifest::default();
    let mut published = None;
    for _ in 0..4 {
        let result = indexer.run_backfill_from_storage(&storage, dir.path(), &mut manifest, plan(1))?;
        assert_eq!(result.embedded_docs, 1);
        if result.published { published = Some(result.index_path); }
    }
    let path = published.context("bounded backfill did not finish")?;
    let inventory = indexer.replay_cache.slot.lock().unwrap();
    assert!(inventory.is_some(), "real engine must call the new planner, not just compile it");
    drop(inventory);
    assert!(manifest.checkpoint.is_none());
    let vectors = FsVectorIndex::open_read_only(&path)?;
    let actual = vectors.live_doc_ids()?;
    let expected: HashSet<_> = packet_embedding_inputs_from_storage(&storage)?.iter()
        .filter_map(semantic_doc_id_for_input).collect();
    assert_eq!(actual, expected);
    assert_eq!(actual.len(), 4);
    // A fresh invocation has no hidden global cache or disk authority.
    assert!(SemanticIndexer::new("hash", None)?.replay_cache.slot.lock().unwrap().is_none());
    Ok(())
}

#[test]
#[cfg(unix)]
fn replay_cache_records_same_invocation_uncached_and_cached_work() -> Result<()> {
    let bodies: Vec<_> = (0..32).map(|i| format!("canonical message number {i}")).collect();
    let refs: Vec<_> = bodies.iter().map(String::as_str).collect();
    let (_dir, storage) = fixture(&refs)?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let cached = cache(MAX_RETAINED_BYTES);
    let uncached = cache_disabled();
    let plan = plan(1);
    let empty = HashSet::new();
    select(&cached, &indexer, &storage, &empty, &plan, SemanticCheckpointCaps::unlimited())?;
    for round in 0..4 {
        let mut previous = None;
        for enabled in if round % 2 == 0 { [false, true] } else { [true, false] } {
            let started = Instant::now();
            let (selection, proof) = select(if enabled { &cached } else { &uncached },
                &indexer, &storage, &empty, &plan, SemanticCheckpointCaps::unlimited())?;
            proof.verify(&storage)?;
            let observed = signature(&selection);
            if let Some(expected) = &previous { assert_eq!(&observed, expected); }
            previous = Some(observed);
            assert_eq!(selection.replayed_conversations, if enabled { 0 } else { 32 });
            eprintln!("{}", json!({"scenario":"canonical_replay_cache", "round":round,
                "cached":enabled, "elapsed_us":started.elapsed().as_micros(),
                "canonical_conversations_replayed":selection.replayed_conversations,
                "selected_conversations_reloaded":selection.loaded_selected_conversations}));
        }
    }
    Ok(())
}
