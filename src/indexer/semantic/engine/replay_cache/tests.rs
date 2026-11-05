//! Real canonical-store and hash/FSVI coverage. No model downloads or verdict mocks.

use super::*;
use crate::model::types::{Agent, AgentKind, MessageRole};
use serde_json::json;
#[cfg(unix)]
use std::time::Instant;

fn fixture(bodies: &[&str]) -> Result<(tempfile::TempDir, FrankenStorage)> {
    let dir = tempfile::tempdir()?;
    let storage = FrankenStorage::open(&dir.path().join("agent_search.db"))?;
    let agent = storage.ensure_agent(&Agent {
        id: None,
        slug: "codex".into(),
        name: "Codex".into(),
        version: None,
        kind: AgentKind::Cli,
    })?;
    for (ordinal, body) in bodies.iter().enumerate() {
        storage.insert_conversation_tree(
            agent,
            None,
            &Conversation {
                id: None,
                agent_slug: "codex".into(),
                workspace: None,
                external_id: Some(format!("replay-{ordinal}")),
                title: Some(format!("replay {ordinal}")),
                source_path: dir.path().join(format!("source-{ordinal}.jsonl")),
                started_at: Some(1_700_000_000_000),
                ended_at: Some(1_700_000_001_000),
                approx_tokens: None,
                metadata_json: json!({}),
                source_id: "local".into(),
                origin_host: None,
                messages: vec![Message {
                    id: None,
                    idx: 0,
                    role: MessageRole::User,
                    author: None,
                    created_at: Some(1_700_000_000_500),
                    content: (*body).into(),
                    extra_json: json!({}),
                    snippets: Vec::new(),
                }],
            },
        )?;
    }
    Ok((dir, storage))
}

fn signed_fixture(parents: &[i64]) -> Result<(tempfile::TempDir, FrankenStorage)> {
    let (dir, storage) = fixture(&[])?;
    let agent: i64 = storage.raw().query_row_map(
        "SELECT id FROM agents WHERE slug = 'codex'",
        &[] as &[ParamValue],
        |row| row.get_typed(0),
    )?;
    for (ordinal, &id) in parents.iter().enumerate() {
        storage.raw().execute_compat(
            "INSERT INTO conversations(id, agent_id, source_id, external_id, source_path)
             VALUES (?1, ?2, 'local', ?3, ?4)",
            &[
                ParamValue::from(id),
                ParamValue::from(agent),
                ParamValue::from(format!("signed-replay-{ordinal}")),
                ParamValue::from(format!("/signed-replay/{ordinal}.jsonl")),
            ],
        )?;
        storage.raw().execute_compat(
            "INSERT INTO messages(conversation_id, idx, role, content, created_at)
             VALUES (?1, 0, 'user', ?2, 1)",
            &[
                ParamValue::from(id),
                ParamValue::from(format!("signed replay evidence {ordinal}")),
            ],
        )?;
    }
    Ok((dir, storage))
}

fn plan(limit: usize) -> SemanticBackfillStoragePlan {
    SemanticBackfillStoragePlan {
        tier: TierKind::Fast,
        db_fingerprint: "replay-cache-test".into(),
        model_revision: "hash".into(),
        max_conversations: limit,
    }
}

fn cache(bytes: usize) -> CanonicalReplayCache {
    CanonicalReplayCache {
        slot: Mutex::new(None),
        max_bytes: bytes,
    }
}

fn signature(selection: &ReconcileSelection) -> Vec<(u64, u8, String)> {
    selection
        .inputs
        .iter()
        .map(|input| {
            (
                input.message_id,
                input.chunk_idx,
                semantic_doc_id_for_input(input).unwrap(),
            )
        })
        .collect()
}

fn select(
    cache: &CanonicalReplayCache,
    indexer: &SemanticIndexer,
    storage: &FrankenStorage,
    existing: &HashSet<String>,
    plan: &SemanticBackfillStoragePlan,
    caps: SemanticCheckpointCaps,
) -> Result<(ReconcileSelection, ReplayProof)> {
    let database_path = storage.database_path()?;
    cache.select(
        indexer,
        storage,
        database_path
            .parent()
            .context("fixture archive has no parent directory")?,
        plan,
        existing,
        caps,
        &SemanticProgressSink::disabled(),
    )
}

#[test]
#[cfg(unix)]
fn replay_cache_reads_the_archive_once_and_only_reloads_selected_text() -> Result<()> {
    let (_dir, storage) = fixture(&[
        "first message",
        "second message",
        "third message",
        "last message",
    ])?;
    assert!(
        BackfillFilePair::archive(&storage).is_some(),
        "fixture needs a descriptor-bound observation"
    );
    let indexer = SemanticIndexer::new("hash", None)?;
    let cache = cache(MAX_RETAINED_BYTES);
    let baseline = cache_disabled();
    let plan = plan(1);
    let mut existing = HashSet::new();
    let mut canonical = None;
    for round in 0..4 {
        let (expected, _) = select(
            &baseline,
            &indexer,
            &storage,
            &existing,
            &plan,
            SemanticCheckpointCaps::unlimited(),
        )?;
        let (actual, proof) = select(
            &cache,
            &indexer,
            &storage,
            &existing,
            &plan,
            SemanticCheckpointCaps::unlimited(),
        )?;
        assert_eq!(signature(&actual), signature(&expected));
        assert_eq!(actual.current_ids, expected.current_ids);
        assert_eq!(actual.covered_conversations, expected.covered_conversations);
        assert_eq!(
            actual.last_covered_conversation,
            expected.last_covered_conversation
        );
        assert_eq!(actual.last_message_id, expected.last_message_id);
        assert_eq!(
            actual.replayed_conversations,
            if round == 0 { 4 } else { 0 }
        );
        assert_eq!(actual.loaded_selected_conversations, 1);
        assert_eq!(actual.inputs[0].message_id, round + 1);
        proof.verify(&storage)?;
        canonical.get_or_insert_with(|| actual.current_ids.clone());
        existing.extend(actual.selected_ids);
    }
    assert_eq!(Some(existing), canonical);
    Ok(())
}

fn cache_disabled() -> CanonicalReplayCache {
    cache(0)
}

#[cfg(unix)]
fn append_fixture_message(storage: &FrankenStorage, id: i64, body: &str) -> Result<()> {
    let mut conversation = storage
        .list_conversations(i64::MAX, 0)?
        .into_iter()
        .find(|conversation| conversation.id == Some(id))
        .context("missing fixture conversation")?;
    conversation.messages = storage.fetch_messages(id)?;
    let idx = conversation
        .messages
        .iter()
        .map(|message| message.idx)
        .max()
        .unwrap_or(-1)
        + 1;
    conversation.messages.push(Message {
        id: None,
        idx,
        role: MessageRole::User,
        author: None,
        created_at: Some(1_700_000_002_000 + idx),
        content: body.into(),
        extra_json: json!({}),
        snippets: Vec::new(),
    });
    let agent: i64 = storage.raw().query_row_map(
        "SELECT agent_id FROM conversations WHERE id = ?1",
        &[ParamValue::from(id)],
        |row| row.get_typed(0),
    )?;
    let outcome =
        storage.insert_conversation_tree_with_analytics(agent, None, &conversation, true)?;
    assert_eq!(outcome.conversation_id, id);
    Ok(())
}

#[cfg(unix)]
fn current_plan(storage: &FrankenStorage, limit: usize) -> Result<SemanticBackfillStoragePlan> {
    Ok(SemanticBackfillStoragePlan {
        db_fingerprint: crate::indexer::lexical_storage_fingerprint_for_storage(storage)?,
        ..plan(limit)
    })
}

#[test]
fn replay_cache_preserves_signed_parents_across_pages_and_budget_modes() -> Result<()> {
    // The first page contains only negative IDs. Later pages cross zero and
    // end at MAX, exercising both cursor bounds without cursor+1 arithmetic.
    let mut parents = vec![i64::MIN];
    parents.extend(-(DEFAULT_SEMANTIC_RECONCILIATION_SCAN_CONVERSATIONS as i64)..=0);
    parents.push(i64::MAX);
    let (_dir, storage) = signed_fixture(&parents)?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let plan = plan(parents.len());
    let expected_contents: Vec<_> = (0..parents.len())
        .map(|ordinal| format!("signed replay evidence {ordinal}"))
        .collect();
    let can_retain = BackfillFilePair::archive(&storage).is_some();
    let mut expected_signature = None;
    for bytes in [0, 1, MAX_RETAINED_BYTES] {
        let cache = cache(bytes);
        for round in 0..2 {
            let (selection, proof) = select(
                &cache,
                &indexer,
                &storage,
                &HashSet::new(),
                &plan,
                SemanticCheckpointCaps::unlimited(),
            )?;
            assert_eq!(
                selection
                    .inputs
                    .iter()
                    .map(|input| input.content.clone())
                    .collect::<Vec<_>>(),
                expected_contents,
                "canonical parents must survive cached, disabled and budget-refused replay"
            );
            assert_eq!(selection.current_ids.len(), parents.len());
            assert_eq!(selection.covered_conversations, parents.len() as u64);
            assert_eq!(selection.last_covered_conversation, i64::MAX);
            let expected_replayed = if round == 1 && bytes == MAX_RETAINED_BYTES && can_retain {
                0
            } else {
                parents.len()
            };
            assert_eq!(selection.replayed_conversations, expected_replayed);
            let observed = signature(&selection);
            if let Some(expected) = &expected_signature {
                assert_eq!(&observed, expected);
            } else {
                expected_signature = Some(observed);
            }
            proof.verify(&storage)?;
        }
    }
    Ok(())
}

#[test]
fn replay_cache_bounded_batches_retain_nonpositive_checkpoint_progress() -> Result<()> {
    let parents = [i64::MIN, -1, 0, i64::MAX];
    let (_dir, storage) = signed_fixture(&parents)?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let cached = cache(MAX_RETAINED_BYTES);
    let uncached = cache_disabled();
    let plan = plan(1);
    let mut existing = HashSet::new();
    for (ordinal, parent) in parents.into_iter().enumerate() {
        let (expected, _) = select(
            &uncached,
            &indexer,
            &storage,
            &existing,
            &plan,
            SemanticCheckpointCaps::unlimited(),
        )?;
        let (actual, proof) = select(
            &cached,
            &indexer,
            &storage,
            &existing,
            &plan,
            SemanticCheckpointCaps::unlimited(),
        )?;
        assert_eq!(signature(&actual), signature(&expected));
        assert_eq!(actual.inputs.len(), 1);
        assert_eq!(
            actual.inputs[0].content,
            format!("signed replay evidence {ordinal}")
        );
        assert_eq!(actual.current_ids.len(), parents.len());
        assert_eq!(actual.covered_conversations, (ordinal + 1) as u64);
        assert_eq!(actual.last_covered_conversation, parent);
        assert_eq!(
            actual.last_covered_conversation,
            expected.last_covered_conversation
        );
        proof.verify(&storage)?;
        existing.extend(actual.selected_ids);
    }
    assert_eq!(existing.len(), parents.len());
    Ok(())
}

#[test]
fn replay_cache_budget_refusal_preserves_the_full_uncached_selection() -> Result<()> {
    let (_dir, storage) = fixture(&["first message", "second message", "third message"])?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let tiny = cache(1);
    let disabled = cache_disabled();
    let plan = plan(2);
    for _ in 0..2 {
        let (actual, _) = select(
            &tiny,
            &indexer,
            &storage,
            &HashSet::new(),
            &plan,
            SemanticCheckpointCaps::unlimited(),
        )?;
        let (expected, _) = select(
            &disabled,
            &indexer,
            &storage,
            &HashSet::new(),
            &plan,
            SemanticCheckpointCaps::unlimited(),
        )?;
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
    let (first, _) = select(
        &cache,
        &indexer,
        &storage,
        &empty,
        &plan,
        SemanticCheckpointCaps::unlimited(),
    )?;
    // Simulate retry after a failed embedding/publication by leaving the REAL
    // caller-supplied coverage set unchanged, not by reporting synthetic success.
    let (retry, _) = select(
        &cache,
        &indexer,
        &storage,
        &empty,
        &plan,
        SemanticCheckpointCaps::unlimited(),
    )?;
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
    let (before, _) = select(
        &cache,
        &indexer,
        &storage,
        &HashSet::new(),
        &plan,
        SemanticCheckpointCaps::unlimited(),
    )?;
    let fingerprint = crate::indexer::lexical_storage_fingerprint_for_storage(&storage)?;
    storage
        .raw()
        .execute("UPDATE messages SET content = 'new content' WHERE conversation_id = 1")?;
    assert_eq!(
        crate::indexer::lexical_storage_fingerprint_for_storage(&storage)?,
        fingerprint
    );
    let (after, _) = select(
        &cache,
        &indexer,
        &storage,
        &before.current_ids,
        &plan,
        SemanticCheckpointCaps::unlimited(),
    )?;
    assert_eq!(after.replayed_conversations, 3);
    assert_eq!(after.inputs.len(), 1);
    assert_eq!(after.inputs[0].content, "new content");
    assert_eq!(
        after.current_ids.intersection(&before.current_ids).count(),
        2
    );
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
    let (before, _) = select(
        &cache,
        &indexer,
        &storage,
        &HashSet::new(),
        &plan,
        SemanticCheckpointCaps::unlimited(),
    )?;
    storage
        .raw()
        .execute("DELETE FROM messages WHERE conversation_id = 1")?;
    storage
        .raw()
        .execute("UPDATE messages SET role = 'assistant' WHERE conversation_id = 2")?;
    let (after, _) = select(
        &cache,
        &indexer,
        &storage,
        &before.current_ids,
        &plan,
        SemanticCheckpointCaps::unlimited(),
    )?;
    assert_eq!(after.replayed_conversations, 3);
    assert_eq!(after.current_ids.len(), 2);
    assert_eq!(after.inputs.len(), 1);
    assert_eq!(after.inputs[0].message_id, 2);
    assert_eq!(
        after.current_ids.intersection(&before.current_ids).count(),
        1
    );
    assert_eq!(
        after.covered_conversations, 3,
        "empty parent still counts as covered"
    );
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
    select(
        &cache,
        &indexer,
        &a,
        &empty,
        &plan,
        SemanticCheckpointCaps::unlimited(),
    )?;
    let (other, _) = select(
        &cache,
        &indexer,
        &b,
        &empty,
        &plan,
        SemanticCheckpointCaps::unlimited(),
    )?;
    assert_eq!(other.replayed_conversations, 1);
    assert_eq!(other.inputs[0].content, "other archive");
    plan.model_revision = "different-producer-contract".into();
    let (changed, _) = select(
        &cache,
        &indexer,
        &b,
        &empty,
        &plan,
        SemanticCheckpointCaps::unlimited(),
    )?;
    assert_eq!(changed.replayed_conversations, 1);
    plan.db_fingerprint = "different-input-contract".into();
    let (changed, _) = select(
        &cache,
        &indexer,
        &b,
        &empty,
        &plan,
        SemanticCheckpointCaps::unlimited(),
    )?;
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
    select(
        &cache,
        &indexer,
        &storage,
        &HashSet::new(),
        &plan,
        SemanticCheckpointCaps::unlimited(),
    )?;
    let (selection, proof) = select(
        &cache,
        &indexer,
        &storage,
        &HashSet::new(),
        &plan,
        SemanticCheckpointCaps::unlimited(),
    )?;
    assert_eq!(
        selection.replayed_conversations, 0,
        "negative requires an actual inventory hit"
    );
    assert!(
        proof.0.is_some(),
        "cached replay must retain its archive observation"
    );
    proof.verify(&storage)?;
    storage.raw().execute(
        "UPDATE messages SET content = 'changed during embedding' WHERE conversation_id = 2",
    )?;
    assert!(
        proof
            .verify(&storage)
            .unwrap_err()
            .to_string()
            .contains("archive changed")
    );
    Ok(())
}

#[test]
#[cfg(unix)]
fn replay_cache_cold_scan_cannot_publish_after_a_later_canonical_write() -> Result<()> {
    // Cold retained, disabled and budget-refused scans all need the same
    // archive fence. Mutate an unselected conversation after selection, just
    // as an ingest can commit while a long embedding batch is running.
    for bytes in [MAX_RETAINED_BYTES, 0, 1] {
        let (_dir, storage) = fixture(&["selected message", "unselected message"])?;
        let indexer = SemanticIndexer::new("hash", None)?;
        let cache = cache(bytes);
        let (selection, proof) = select(
            &cache,
            &indexer,
            &storage,
            &HashSet::new(),
            &plan(1),
            SemanticCheckpointCaps::unlimited(),
        )?;
        assert_eq!(selection.replayed_conversations, 2);
        assert_eq!(selection.inputs.len(), 1);
        assert_eq!(selection.inputs[0].content, "selected message");
        assert!(proof.0.is_some(), "cold replay must retain its observation");
        proof.verify(&storage)?;
        storage.raw().execute(
            "UPDATE messages SET content = 'changed during embedding' WHERE conversation_id = 2",
        )?;
        assert!(
            proof
                .verify(&storage)
                .unwrap_err()
                .to_string()
                .contains("archive changed")
        );
    }
    Ok(())
}

#[test]
fn replay_cache_preserves_later_small_fit_and_first_oversized_conversation() -> Result<()> {
    let large = "longword ".repeat(20);
    let (_dir, storage) = fixture(&["small one", &large, "small two"])?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let cache = cache(MAX_RETAINED_BYTES);
    let plan = plan(3);
    let caps = SemanticCheckpointCaps {
        max_messages: 0,
        max_bytes: 32,
    };
    for _ in 0..2 {
        let (selection, _) = select(&cache, &indexer, &storage, &HashSet::new(), &plan, caps)?;
        assert_eq!(
            selection
                .inputs
                .iter()
                .map(|input| input.message_id)
                .collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert_eq!(selection.covered_conversations, 2);
        let (oversized, _) = select(
            &cache,
            &indexer,
            &storage,
            &selection.selected_ids,
            &plan,
            caps,
        )?;
        assert_eq!(
            oversized
                .inputs
                .iter()
                .map(|input| input.message_id)
                .collect::<Vec<_>>(),
            vec![2]
        );
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
    select(
        &cache,
        &indexer,
        &storage,
        &HashSet::new(),
        &plan,
        SemanticCheckpointCaps::unlimited(),
    )?;
    storage.raw().execute("BEGIN")?;
    let result = select(
        &cache,
        &indexer,
        &storage,
        &HashSet::new(),
        &plan,
        SemanticCheckpointCaps::unlimited(),
    );
    storage.raw().execute("ROLLBACK")?;
    let (selected, proof) = result?;
    assert_eq!(selected.replayed_conversations, 1);
    assert!(proof.0.is_none());
    assert!(cache.slot.lock().unwrap().is_none());
    Ok(())
}

#[test]
#[cfg(unix)]
fn replay_cache_live_backfill_converges_with_identical_published_fsvi_coverage() -> Result<()> {
    let (dir, storage) = fixture(&[
        "first message",
        "second message",
        "third message",
        "last message",
    ])?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let mut manifest = SemanticManifest::default();
    let mut published = None;
    for _ in 0..4 {
        let result =
            indexer.run_backfill_from_storage(&storage, dir.path(), &mut manifest, plan(1))?;
        assert_eq!(result.embedded_docs, 1);
        if result.published {
            published = Some(result.index_path);
        }
    }
    let path = published.context("bounded backfill did not finish")?;
    let inventory = indexer.replay_cache.slot.lock().unwrap();
    assert!(
        inventory.is_some(),
        "real engine must call the new planner, not just compile it"
    );
    drop(inventory);
    assert!(manifest.checkpoint.is_none());
    let vectors = FsVectorIndex::open_read_only(&path)?;
    let actual = vectors.live_doc_ids()?;
    let expected: HashSet<_> = packet_embedding_inputs_from_storage(&storage)?
        .iter()
        .filter_map(semantic_doc_id_for_input)
        .collect();
    assert_eq!(actual, expected);
    assert_eq!(actual.len(), 4);
    // A fresh indexer has no hidden global memory slot. Any reuse must load
    // the bounded disk inventory and revalidate it against the open archive.
    assert!(
        SemanticIndexer::new("hash", None)?
            .replay_cache
            .slot
            .lock()
            .unwrap()
            .is_none()
    );
    Ok(())
}

#[test]
#[cfg(unix)]
fn replay_cache_records_same_invocation_uncached_and_cached_work() -> Result<()> {
    let bodies: Vec<_> = (0..32)
        .map(|i| format!("canonical message number {i}"))
        .collect();
    let refs: Vec<_> = bodies.iter().map(String::as_str).collect();
    let (_dir, storage) = fixture(&refs)?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let cached = cache(MAX_RETAINED_BYTES);
    let uncached = cache_disabled();
    let plan = plan(1);
    let empty = HashSet::new();
    select(
        &cached,
        &indexer,
        &storage,
        &empty,
        &plan,
        SemanticCheckpointCaps::unlimited(),
    )?;
    for round in 0..4 {
        let mut previous = None;
        for enabled in if round % 2 == 0 {
            [false, true]
        } else {
            [true, false]
        } {
            let started = Instant::now();
            let (selection, proof) = select(
                if enabled { &cached } else { &uncached },
                &indexer,
                &storage,
                &empty,
                &plan,
                SemanticCheckpointCaps::unlimited(),
            )?;
            proof.verify(&storage)?;
            let observed = signature(&selection);
            if let Some(expected) = &previous {
                assert_eq!(&observed, expected);
            }
            previous = Some(observed);
            assert_eq!(
                selection.replayed_conversations,
                if enabled { 0 } else { 32 }
            );
            eprintln!(
                "{}",
                json!({"scenario":"canonical_replay_cache", "round":round,
                "cached":enabled, "elapsed_us":started.elapsed().as_micros(),
                "canonical_conversations_replayed":selection.replayed_conversations,
                "selected_conversations_reloaded":selection.loaded_selected_conversations})
            );
        }
    }
    Ok(())
}

#[test]
#[cfg(unix)]
fn replay_cache_fresh_indexer_reuses_disk_inventory_without_rewriting_it() -> Result<()> {
    let (dir, storage) = fixture(&["first message", "second message", "third message"])?;
    let plan = plan(1);
    let indexer = SemanticIndexer::new("hash", None)?;
    let first_cache = cache(MAX_RETAINED_BYTES);
    let (first, _) = select(
        &first_cache,
        &indexer,
        &storage,
        &HashSet::new(),
        &plan,
        SemanticCheckpointCaps::unlimited(),
    )?;
    assert_eq!(first.replayed_conversations, 3);
    let path = persistent_path(dir.path(), plan.tier);
    let stamp = backfill_file_stamp(&path)
        .flatten()
        .context("persisted inventory")?;
    drop(first_cache);
    drop(indexer);

    let fresh_indexer = SemanticIndexer::new("hash", None)?;
    let fresh_cache = cache(MAX_RETAINED_BYTES);
    let (next, proof) = select(
        &fresh_cache,
        &fresh_indexer,
        &storage,
        &first.selected_ids,
        &plan,
        SemanticCheckpointCaps::unlimited(),
    )?;
    let (expected, _) = select(
        &cache_disabled(),
        &fresh_indexer,
        &storage,
        &first.selected_ids,
        &plan,
        SemanticCheckpointCaps::unlimited(),
    )?;
    assert_eq!(next.replayed_conversations, 0);
    assert_eq!(next.loaded_selected_conversations, 1);
    assert_eq!(signature(&next), signature(&expected));
    assert_eq!(next.current_ids, expected.current_ids);
    assert_eq!(next.inputs[0].content, "second message");
    proof.verify(&storage)?;
    assert_eq!(backfill_file_stamp(&path).flatten(), Some(stamp));
    Ok(())
}

#[test]
#[cfg(unix)]
fn replay_cache_streams_inventory_above_memory_budget_without_replaying_bodies() -> Result<()> {
    let bodies: Vec<_> = (0..64)
        .map(|ordinal| format!("streamed canonical evidence number {ordinal}"))
        .collect();
    let refs: Vec<_> = bodies.iter().map(String::as_str).collect();
    let (dir, storage) = fixture(&refs)?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let plan = plan(2);
    let budget = 1024;
    let first_cache = cache(budget);
    let (first, _) = select(
        &first_cache,
        &indexer,
        &storage,
        &HashSet::new(),
        &plan,
        SemanticCheckpointCaps::unlimited(),
    )?;
    assert_eq!(first.replayed_conversations, bodies.len());
    assert_eq!(first.inputs.len(), 2);
    assert!(first_cache.slot.lock().unwrap().is_none());
    let path = persistent_path(dir.path(), plan.tier);
    let stamp = backfill_file_stamp(&path)
        .flatten()
        .context("oversized inventory was not persisted")?;
    assert!(fs::metadata(&path)?.len() > budget as u64);

    let fresh_cache = cache(budget);
    let (actual, proof) = select(
        &fresh_cache,
        &indexer,
        &storage,
        &first.selected_ids,
        &plan,
        SemanticCheckpointCaps::unlimited(),
    )?;
    let (expected, _) = select(
        &cache_disabled(),
        &indexer,
        &storage,
        &first.selected_ids,
        &plan,
        SemanticCheckpointCaps::unlimited(),
    )?;
    assert_eq!(actual.replayed_conversations, 0);
    assert_eq!(actual.loaded_selected_conversations, 2);
    assert_eq!(signature(&actual), signature(&expected));
    assert_eq!(actual.current_ids, expected.current_ids);
    assert_eq!(actual.covered_conversations, expected.covered_conversations);
    assert!(fresh_cache.slot.lock().unwrap().is_none());
    assert_eq!(backfill_file_stamp(&path).flatten(), Some(stamp));
    proof.verify(&storage)?;

    storage.raw().execute(
        "UPDATE messages SET content = 'changed after streamed selection' WHERE conversation_id = 64",
    )?;
    assert!(proof.verify(&storage).is_err());
    let (changed, proof) = select(
        &cache(budget),
        &indexer,
        &storage,
        &first.current_ids,
        &plan,
        SemanticCheckpointCaps::unlimited(),
    )?;
    assert_eq!(changed.replayed_conversations, bodies.len());
    assert_eq!(changed.inputs.len(), 1);
    assert_eq!(
        changed.inputs[0].content,
        "changed after streamed selection"
    );
    proof.verify(&storage)?;
    Ok(())
}

#[test]
#[cfg(unix)]
fn replay_cache_corrupt_large_stream_discards_all_tentative_selection() -> Result<()> {
    let bodies: Vec<_> = (0..32)
        .map(|ordinal| format!("stream checksum evidence number {ordinal}"))
        .collect();
    let refs: Vec<_> = bodies.iter().map(String::as_str).collect();
    let (dir, storage) = fixture(&refs)?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let plan = plan(3);
    let budget = 1024;
    let (first, _) = select(
        &cache(budget),
        &indexer,
        &storage,
        &HashSet::new(),
        &plan,
        SemanticCheckpointCaps::unlimited(),
    )?;
    let path = persistent_path(dir.path(), plan.tier);
    let original = fs::read(&path)?;
    assert!(original.len() > budget);
    // Both faults are discovered after every frame was consumed and the
    // selected conversations' text was loaded. No prefix may survive retry.
    for trailing_byte in [false, true] {
        let mut damaged = original.clone();
        if trailing_byte {
            damaged.push(0);
        } else {
            *damaged.last_mut().context("inventory checksum")? ^= 1;
        }
        fs::write(&path, damaged)?;
        let (actual, proof) = select(
            &cache(budget),
            &indexer,
            &storage,
            &first.selected_ids,
            &plan,
            SemanticCheckpointCaps::unlimited(),
        )?;
        let (expected, _) = select(
            &cache_disabled(),
            &indexer,
            &storage,
            &first.selected_ids,
            &plan,
            SemanticCheckpointCaps::unlimited(),
        )?;
        assert_eq!(actual.replayed_conversations, bodies.len());
        assert_eq!(actual.loaded_selected_conversations, 3);
        assert_eq!(actual.selected_conversations, 3);
        assert_eq!(signature(&actual), signature(&expected));
        assert_eq!(actual.current_ids, expected.current_ids);
        assert_eq!(actual.selected_ids, expected.selected_ids);
        proof.verify(&storage)?;
    }
    Ok(())
}

#[test]
#[cfg(unix)]
fn replay_cache_records_same_invocation_uncached_and_streamed_selection_work() -> Result<()> {
    let bodies: Vec<_> = (0..128)
        .map(|ordinal| format!("canonical stream measurement {ordinal} ").repeat(64))
        .collect();
    let refs: Vec<_> = bodies.iter().map(String::as_str).collect();
    let (_dir, storage) = fixture(&refs)?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let cached = cache(4096);
    let uncached = cache_disabled();
    let plan = plan(1);
    let empty = HashSet::new();
    select(
        &cached,
        &indexer,
        &storage,
        &empty,
        &plan,
        SemanticCheckpointCaps::unlimited(),
    )?;
    assert!(cached.slot.lock().unwrap().is_none());
    for round in 0..4 {
        let mut previous = None;
        for enabled in if round % 2 == 0 {
            [false, true]
        } else {
            [true, false]
        } {
            let started = Instant::now();
            let (selection, proof) = select(
                if enabled { &cached } else { &uncached },
                &indexer,
                &storage,
                &empty,
                &plan,
                SemanticCheckpointCaps::unlimited(),
            )?;
            proof.verify(&storage)?;
            let observed = signature(&selection);
            if let Some(expected) = &previous {
                assert_eq!(&observed, expected);
            }
            previous = Some(observed);
            assert_eq!(
                selection.replayed_conversations,
                if enabled { 0 } else { 128 }
            );
            assert!(cached.slot.lock().unwrap().is_none());
            eprintln!(
                "{}",
                json!({"scenario":"streamed_canonical_replay_cache", "round":round,
                "cached":enabled, "elapsed_us":started.elapsed().as_micros(),
                "canonical_conversations_replayed":selection.replayed_conversations,
                "selected_conversations_reloaded":selection.loaded_selected_conversations,
                "retained_cache_bytes":0})
            );
        }
    }
    Ok(())
}

#[test]
#[cfg(unix)]
fn replay_cache_journal_merges_appended_new_and_forgotten_conversations() -> Result<()> {
    let bodies: Vec<_> = (0..64)
        .map(|id| format!("journal fixture conversation {id}"))
        .collect();
    let refs: Vec<_> = bodies.iter().map(String::as_str).collect();
    for budget in [MAX_RETAINED_BYTES, 1024] {
        let (dir, storage) = fixture(&refs)?;
        let indexer = SemanticIndexer::new("hash", None)?;
        let cache = cache(budget);
        let initial_plan = current_plan(&storage, 128)?;
        let before_state = storage
            .semantic_archive_state()?
            .context("fixture journal")?;
        let (before, _) = select(
            &cache,
            &indexer,
            &storage,
            &HashSet::new(),
            &initial_plan,
            SemanticCheckpointCaps::unlimited(),
        )?;
        assert_eq!(before.replayed_conversations, 64);
        assert_eq!(before.current_ids.len(), 64);
        append_fixture_message(&storage, 2, "new message in existing conversation")?;
        let forgotten = storage.forget_conversations_by_source_glob(
            &dir.path().join("source-3.jsonl").to_string_lossy(),
            false,
        )?;
        assert_eq!(forgotten.conversations_deleted, 1);
        let mut added = storage
            .list_conversations(1, 0)?
            .pop()
            .context("new conversation template")?;
        added.id = None;
        added.external_id = Some("journal-new-parent".into());
        added.source_path = dir.path().join("new-parent.jsonl");
        added.messages = vec![Message {
            id: None,
            idx: 0,
            role: MessageRole::User,
            author: None,
            created_at: Some(1_700_000_003_000),
            content: "new conversation message".into(),
            extra_json: json!({}),
            snippets: Vec::new(),
        }];
        let agent: i64 = storage.raw().query_row_map(
            "SELECT id FROM agents WHERE slug = 'codex'",
            &[] as &[ParamValue],
            |row| row.get_typed(0),
        )?;
        let added = storage.insert_conversation_tree_with_analytics(agent, None, &added, true)?;
        let changes = storage
            .semantic_conversation_changes_since(&before_state)?
            .context("complete journal")?;
        assert!(changes.complete);
        assert_eq!(changes.conversation_ids, [2, 4, added.conversation_id]);
        let plan = current_plan(&storage, 2)?;
        assert_ne!(initial_plan.db_fingerprint, plan.db_fingerprint);
        let (actual, proof) = select(
            &cache,
            &indexer,
            &storage,
            &before.current_ids,
            &plan,
            SemanticCheckpointCaps::unlimited(),
        )?;
        let (expected, _) = select(
            &cache_disabled(),
            &indexer,
            &storage,
            &before.current_ids,
            &plan,
            SemanticCheckpointCaps::unlimited(),
        )?;
        assert_eq!(signature(&actual), signature(&expected));
        assert_eq!(actual.current_ids, expected.current_ids);
        assert_eq!(actual.current_ids.len(), 65);
        assert_eq!(actual.selected_ids, expected.selected_ids);
        assert_eq!(actual.covered_conversations, expected.covered_conversations);
        assert_eq!(
            actual.covered_conversations, 64,
            "deleted parents do not count as empty live parents"
        );
        assert_eq!(actual.last_message_id, expected.last_message_id);
        assert_eq!(
            actual.replayed_conversations, 2,
            "only dirty live parents reload canonical bodies"
        );
        assert_eq!(actual.loaded_selected_conversations, 2);
        assert_eq!(expected.replayed_conversations, 64);
        proof.verify(&storage)?;
        if budget == 1024 {
            assert!(cache.slot.lock().unwrap().is_none());
        }
        let key = ReplayKey::capture(&indexer, &storage, &plan)?.context("current key")?;
        let mut persisted = persist::InventoryReader::open(
            &persistent_path(dir.path(), plan.tier),
            &key,
            budget,
            || Ok(()),
        )?
        .context("merged inventory")?;
        let mut parents = Vec::new();
        while let Some(conversation) = persisted.next_conversation(|| Ok(()))? {
            parents.push(conversation.conversation_id);
        }
        assert_eq!(parents.len(), 64);
        assert!(!parents.contains(&4));
        assert!(parents.contains(&added.conversation_id));
        let (unchanged, _) = select(
            &CanonicalReplayCache {
                slot: Mutex::new(None),
                max_bytes: budget,
            },
            &indexer,
            &storage,
            &actual.current_ids,
            &plan,
            SemanticCheckpointCaps::unlimited(),
        )?;
        assert_eq!(unchanged.replayed_conversations, 0);
        assert!(unchanged.inputs.is_empty());
        append_fixture_message(&storage, 3, "ingested after selection")?;
        assert!(
            proof.verify(&storage).is_err(),
            "journal reuse keeps the exact publication race fence"
        );
    }
    Ok(())
}

#[test]
#[cfg(unix)]
fn replay_cache_journal_missing_foreign_regressed_global_and_producer_changes_fall_back()
-> Result<()> {
    for scenario in [
        "missing",
        "foreign",
        "regressed",
        "global",
        "missing_history",
        "producer",
        "identity",
    ] {
        let (_dir, storage) = fixture(&["one", "two", "three", "four"])?;
        let indexer = SemanticIndexer::new("hash", None)?;
        let before_plan = current_plan(&storage, 4)?;
        let before_state = storage
            .semantic_archive_state()?
            .context("fixture journal")?;
        let (before, _) = select(
            &cache(1024),
            &indexer,
            &storage,
            &HashSet::new(),
            &before_plan,
            SemanticCheckpointCaps::unlimited(),
        )?;
        append_fixture_message(&storage, 3, "journal fallback message")?;
        let mut state = storage
            .semantic_archive_state()?
            .context("advanced journal")?;
        match scenario {
            "missing" => {
                storage
                    .raw()
                    .execute("DELETE FROM meta WHERE key = 'semantic_archive_state_v1'")?;
            }
            "foreign" | "regressed" => {
                if scenario == "foreign" {
                    state.archive_id = "f".repeat(32);
                } else {
                    state.mutation_seq = before_state.mutation_seq - 1;
                }
                storage.raw().execute_compat(
                    "UPDATE meta SET value = ?1 WHERE key = 'semantic_archive_state_v1'",
                    &[ParamValue::from(serde_json::to_string(&state)?)],
                )?;
            }
            "global" => {
                storage.raw().execute_compat(
                    "INSERT OR REPLACE INTO semantic_conversation_changes(conversation_id, mutation_seq, invalidation_seq) VALUES(0, ?1, ?1)",
                    &[ParamValue::from(i64::try_from(state.mutation_seq)?)])?;
            }
            "missing_history" => {
                storage
                    .raw()
                    .execute("DELETE FROM semantic_conversation_changes")?;
            }
            _ => {}
        }
        let mut plan = current_plan(&storage, 4)?;
        if scenario == "producer" {
            plan.model_revision = "different-producer".into();
        }
        if scenario == "identity" {
            plan.db_fingerprint = format!("identity-v1:replacement:{}", plan.db_fingerprint);
        }
        let (actual, proof) = select(
            &cache(1024),
            &indexer,
            &storage,
            &before.current_ids,
            &plan,
            SemanticCheckpointCaps::unlimited(),
        )?;
        let (expected, _) = select(
            &cache_disabled(),
            &indexer,
            &storage,
            &before.current_ids,
            &plan,
            SemanticCheckpointCaps::unlimited(),
        )?;
        assert_eq!(actual.replayed_conversations, 4, "scenario {scenario}");
        assert_eq!(
            actual.current_ids, expected.current_ids,
            "scenario {scenario}"
        );
        assert_eq!(
            signature(&actual),
            signature(&expected),
            "scenario {scenario}"
        );
        assert_eq!(actual.inputs.len(), 1);
        proof.verify(&storage)?;
    }
    Ok(())
}

#[test]
#[cfg(unix)]
fn replay_cache_journal_corrupt_old_stream_discards_the_entire_merged_selection() -> Result<()> {
    let bodies: Vec<_> = (0..32)
        .map(|id| format!("merged integrity fixture {id}"))
        .collect();
    let refs: Vec<_> = bodies.iter().map(String::as_str).collect();
    let (dir, storage) = fixture(&refs)?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let plan = current_plan(&storage, 32)?;
    let (before, _) = select(
        &cache(1024),
        &indexer,
        &storage,
        &HashSet::new(),
        &plan,
        SemanticCheckpointCaps::unlimited(),
    )?;
    let path = persistent_path(dir.path(), plan.tier);
    let mut damaged = fs::read(&path)?;
    *damaged.last_mut().context("checksum byte")? ^= 1;
    fs::write(&path, damaged)?;
    append_fixture_message(&storage, 1, "dirty prefix must not escape a bad checksum")?;
    let plan = current_plan(&storage, 1)?;
    let (actual, proof) = select(
        &cache(1024),
        &indexer,
        &storage,
        &before.current_ids,
        &plan,
        SemanticCheckpointCaps::unlimited(),
    )?;
    let (expected, _) = select(
        &cache_disabled(),
        &indexer,
        &storage,
        &before.current_ids,
        &plan,
        SemanticCheckpointCaps::unlimited(),
    )?;
    assert_eq!(actual.replayed_conversations, 32);
    assert_eq!(actual.loaded_selected_conversations, 1);
    assert_eq!(actual.current_ids, expected.current_ids);
    assert_eq!(actual.selected_ids, expected.selected_ids);
    assert_eq!(signature(&actual), signature(&expected));
    assert_eq!(actual.inputs.len(), 1);
    proof.verify(&storage)?;
    Ok(())
}

#[test]
#[cfg(unix)]
fn replay_cache_records_same_invocation_uncached_and_journal_selection_work() -> Result<()> {
    let bodies: Vec<_> = (0..128)
        .map(|id| format!("journal canonical measurement {id} ").repeat(64))
        .collect();
    let refs: Vec<_> = bodies.iter().map(String::as_str).collect();
    let (_dir, storage) = fixture(&refs)?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let cached = cache(4096);
    let uncached = cache_disabled();
    let (initial, _) = select(
        &cached,
        &indexer,
        &storage,
        &HashSet::new(),
        &current_plan(&storage, 128)?,
        SemanticCheckpointCaps::unlimited(),
    )?;
    let mut existing = initial.current_ids;
    assert!(cached.slot.lock().unwrap().is_none());
    for round in 0..4 {
        append_fixture_message(
            &storage,
            round + 1,
            &format!("journal appended message {round}"),
        )?;
        let plan = current_plan(&storage, 1)?;
        let mut previous = None;
        let mut current_ids = None;
        for enabled in if round % 2 == 0 {
            [false, true]
        } else {
            [true, false]
        } {
            let started = Instant::now();
            let (selection, proof) = select(
                if enabled { &cached } else { &uncached },
                &indexer,
                &storage,
                &existing,
                &plan,
                SemanticCheckpointCaps::unlimited(),
            )?;
            proof.verify(&storage)?;
            let observed = signature(&selection);
            if let Some(expected) = &previous {
                assert_eq!(&observed, expected);
            }
            if let Some(expected) = &current_ids {
                assert_eq!(&selection.current_ids, expected);
            }
            previous = Some(observed);
            current_ids = Some(selection.current_ids);
            assert_eq!(
                selection.replayed_conversations,
                if enabled { 1 } else { 128 }
            );
            assert_eq!(selection.loaded_selected_conversations, 1);
            assert!(cached.slot.lock().unwrap().is_none());
            eprintln!(
                "{}",
                json!({"scenario":"journal_canonical_replay_cache", "round":round,
                "cached":enabled, "elapsed_us":started.elapsed().as_micros(),
                "canonical_conversations_replayed":selection.replayed_conversations,
                "selected_conversations_reloaded":selection.loaded_selected_conversations,
                "retained_cache_bytes":0})
            );
        }
        existing = current_ids.context("paired journal result")?;
    }
    Ok(())
}

#[test]
#[cfg(unix)]
fn replay_cache_persisted_inventory_rejects_edits_deletions_and_producer_changes() -> Result<()> {
    let (_dir, storage) = fixture(&["old content", "deleted content", "retained content"])?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let mut plan = plan(3);
    let (before, _) = select(
        &cache(MAX_RETAINED_BYTES),
        &indexer,
        &storage,
        &HashSet::new(),
        &plan,
        SemanticCheckpointCaps::unlimited(),
    )?;
    let fingerprint = crate::indexer::lexical_storage_fingerprint_for_storage(&storage)?;
    storage
        .raw()
        .execute("UPDATE messages SET content = 'new content' WHERE conversation_id = 1")?;
    assert_eq!(
        crate::indexer::lexical_storage_fingerprint_for_storage(&storage)?,
        fingerprint
    );
    let (changed, _) = select(
        &cache(MAX_RETAINED_BYTES),
        &indexer,
        &storage,
        &before.current_ids,
        &plan,
        SemanticCheckpointCaps::unlimited(),
    )?;
    assert_eq!(changed.replayed_conversations, 3);
    assert_eq!(changed.inputs.len(), 1);
    assert_eq!(changed.inputs[0].content, "new content");
    storage
        .raw()
        .execute("DELETE FROM messages WHERE conversation_id = 2")?;
    let (deleted, _) = select(
        &cache(MAX_RETAINED_BYTES),
        &indexer,
        &storage,
        &changed.current_ids,
        &plan,
        SemanticCheckpointCaps::unlimited(),
    )?;
    assert_eq!(deleted.replayed_conversations, 3);
    assert_eq!(deleted.current_ids.len(), 2);
    assert!(deleted.inputs.is_empty());
    assert_eq!(deleted.covered_conversations, 3);
    plan.model_revision = "replacement-producer".into();
    let (producer, _) = select(
        &cache(MAX_RETAINED_BYTES),
        &indexer,
        &storage,
        &HashSet::new(),
        &plan,
        SemanticCheckpointCaps::unlimited(),
    )?;
    assert_eq!(producer.replayed_conversations, 3);
    assert_eq!(producer.current_ids, deleted.current_ids);
    Ok(())
}

#[test]
#[cfg(unix)]
fn replay_cache_corrupt_inventory_falls_back_to_complete_canonical_selection() -> Result<()> {
    let (dir, storage) = fixture(&["first message", "second message"])?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let plan = plan(1);
    let (first, _) = select(
        &cache(MAX_RETAINED_BYTES),
        &indexer,
        &storage,
        &HashSet::new(),
        &plan,
        SemanticCheckpointCaps::unlimited(),
    )?;
    let path = persistent_path(dir.path(), plan.tier);
    let mut bytes = fs::read(&path)?;
    *bytes.last_mut().context("inventory checksum")? ^= 1;
    fs::write(&path, bytes)?;
    let (recovered, proof) = select(
        &cache(MAX_RETAINED_BYTES),
        &indexer,
        &storage,
        &first.selected_ids,
        &plan,
        SemanticCheckpointCaps::unlimited(),
    )?;
    assert_eq!(recovered.replayed_conversations, 2);
    assert_eq!(recovered.current_ids, first.current_ids);
    assert_eq!(recovered.inputs.len(), 1);
    assert_eq!(recovered.inputs[0].content, "second message");
    proof.verify(&storage)?;
    // The canonical rescan replaces the damaged cache; the next fresh instance
    // can reuse it, without treating the earlier selected input as coverage.
    let (retry, _) = select(
        &cache(MAX_RETAINED_BYTES),
        &indexer,
        &storage,
        &first.selected_ids,
        &plan,
        SemanticCheckpointCaps::unlimited(),
    )?;
    assert_eq!(retry.replayed_conversations, 0);
    assert_eq!(signature(&retry), signature(&recovered));
    Ok(())
}

#[test]
#[cfg(unix)]
fn replay_cache_fresh_backfill_instances_converge_to_exact_published_coverage() -> Result<()> {
    let (dir, storage) = fixture(&[
        "first message",
        "second message",
        "third message",
        "last message",
    ])?;
    let path = persistent_path(dir.path(), TierKind::Fast);
    let mut inventory_stamp = None;
    let mut published = None;
    for batch in 0..4 {
        let mut manifest = SemanticManifest::load_or_default(dir.path())?;
        let indexer = SemanticIndexer::new("hash", None)?;
        let outcome =
            indexer.run_backfill_from_storage(&storage, dir.path(), &mut manifest, plan(1))?;
        assert_eq!(outcome.embedded_docs, 1);
        if batch == 1 {
            inventory_stamp = backfill_file_stamp(&path).flatten();
            assert!(inventory_stamp.is_some());
        } else if batch > 1 {
            assert_eq!(backfill_file_stamp(&path).flatten(), inventory_stamp);
        }
        if outcome.published {
            assert_eq!(batch, 3);
            published = Some(outcome.index_path);
        }
    }
    let manifest = SemanticManifest::load_or_default(dir.path())?;
    assert!(manifest.checkpoint.is_none());
    let path = published.context("fresh bounded invocations did not publish")?;
    let vectors = FsVectorIndex::open_read_only(&path)?;
    let expected: HashSet<_> = packet_embedding_inputs_from_storage(&storage)?
        .iter()
        .filter_map(semantic_doc_id_for_input)
        .collect();
    assert_eq!(vectors.live_doc_ids()?, expected);
    assert_eq!(expected.len(), 4);
    Ok(())
}
