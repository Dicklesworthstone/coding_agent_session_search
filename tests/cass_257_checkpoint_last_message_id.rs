//! Tests for cass#257 sub-fix 2: per-message `last_message_id`
//! checkpoint cursor with durable resume.
//!
//! Direct API tests (no CLI) — they exercise
//! `SemanticIndexer::run_backfill_from_storage_with_sink` against a
//! tiny seeded corpus, verify that the saved checkpoint carries
//! `last_message_id`, and verify that a second run advances past
//! that cursor rather than re-embedding earlier messages.
//!
//! Also covers the forward-compat fallback: a v1-shape manifest
//! (no `last_message_id`) loads cleanly and the run falls back to
//! the conversation offset.

use std::error::Error;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use coding_agent_search::indexer::semantic::{SemanticBackfillStoragePlan, SemanticIndexer};
use coding_agent_search::indexer::semantic_progress::SemanticProgressSink;
use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use coding_agent_search::search::semantic_manifest::{SemanticManifest, TierKind};
use coding_agent_search::storage::sqlite::FrankenStorage;
use serde_json::{Value, json};
use tempfile::TempDir;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

fn sample_agent() -> Agent {
    Agent {
        id: None,
        slug: "codex".to_string(),
        name: "Codex".to_string(),
        version: None,
        kind: AgentKind::Cli,
    }
}

fn sample_conversation(external_id: &str, content: &str) -> Conversation {
    Conversation {
        id: None,
        agent_slug: "codex".to_string(),
        workspace: None,
        external_id: Some(external_id.to_string()),
        title: Some(format!("cass-257 sub-fix 2 {external_id}")),
        source_path: PathBuf::from(format!("/tmp/cass-e2e/{external_id}.jsonl")),
        started_at: Some(1_700_000_000_000),
        ended_at: Some(1_700_000_001_000),
        approx_tokens: None,
        metadata_json: json!({"fixture": "cass-257-last-message-id"}),
        messages: vec![Message {
            id: None,
            idx: 0,
            role: MessageRole::User,
            author: None,
            created_at: Some(1_700_000_000_500),
            content: content.to_string(),
            extra_json: json!({}),
            snippets: Vec::new(),
        }],
        source_id: "local".to_string(),
        origin_host: None,
    }
}

fn seed_three_conversations(db_path: &Path) -> TestResult<FrankenStorage> {
    let storage = FrankenStorage::open(db_path)?;
    let agent_id = storage.ensure_agent(&sample_agent())?;
    storage.insert_conversation_tree(
        agent_id,
        None,
        &sample_conversation("a", "first conversation a"),
    )?;
    storage.insert_conversation_tree(
        agent_id,
        None,
        &sample_conversation("b", "second conversation b"),
    )?;
    storage.insert_conversation_tree(
        agent_id,
        None,
        &sample_conversation("c", "third conversation c"),
    )?;
    Ok(storage)
}

fn current_db_fingerprint(_db_path: &Path) -> TestResult<String> {
    // We don't go through the production fingerprint helper here because
    // it's `pub(crate)`. The fingerprint only needs to be stable within
    // the test process; the backfill code just compares it to the saved
    // string. Use a marker string so a future audit can grep for "cass-257"
    // and find this test scaffold.
    Ok("cass-257-checkpoint-test-fp".to_string())
}

#[test]
fn checkpoint_persists_last_message_id_after_partial_backfill() -> TestResult {
    let workdir = TempDir::new()?;
    let data_dir = workdir.path().join("data");
    fs::create_dir_all(&data_dir)?;
    let db_path = workdir.path().join("agent_search.db");
    let storage = seed_three_conversations(&db_path)?;
    let db_fingerprint = current_db_fingerprint(&db_path)?;

    let indexer = SemanticIndexer::new("hash", None)?;
    let mut manifest = SemanticManifest::default();
    // Process exactly 1 conversation per call; we expect a checkpoint
    // to land after the first call (2 conversations still pending).
    let outcome = indexer.run_backfill_from_storage_with_sink(
        &storage,
        &data_dir,
        &mut manifest,
        SemanticBackfillStoragePlan {
            tier: TierKind::Fast,
            db_fingerprint: db_fingerprint.clone(),
            model_revision: "hash".to_string(),
            max_conversations: 1,
        },
        &SemanticProgressSink::disabled(),
    )?;
    assert!(!outcome.published, "expected checkpoint, got publish");
    assert!(outcome.checkpoint_saved, "expected checkpoint_saved=true");

    let saved_checkpoint = manifest
        .checkpoint
        .as_ref()
        .expect("checkpoint should be saved after a partial batch");
    let saved_last_message_id = saved_checkpoint
        .last_message_id
        .expect("last_message_id must be populated by sub-fix 2");
    assert!(
        saved_last_message_id > 0,
        "last_message_id must be a positive PK; got {saved_last_message_id}"
    );

    // Round-trip through disk to prove the on-disk JSON shape includes
    // the field.
    let path = SemanticManifest::path(&data_dir);
    let raw = fs::read_to_string(&path)?;
    let on_disk: Value = serde_json::from_str(&raw)?;
    let checkpoint = on_disk
        .get("checkpoint")
        .and_then(Value::as_object)
        .expect("manifest.checkpoint should be a JSON object on disk");
    let on_disk_last_message_id = checkpoint
        .get("last_message_id")
        .and_then(Value::as_i64)
        .expect("manifest.checkpoint.last_message_id must round-trip via serde_json");
    assert_eq!(on_disk_last_message_id, saved_last_message_id);

    Ok(())
}

#[test]
fn resume_skips_messages_with_id_at_or_below_last_message_id() -> TestResult {
    let workdir = TempDir::new()?;
    let data_dir = workdir.path().join("data");
    fs::create_dir_all(&data_dir)?;
    let db_path = workdir.path().join("agent_search.db");
    let storage = seed_three_conversations(&db_path)?;
    let db_fingerprint = current_db_fingerprint(&db_path)?;

    let indexer = SemanticIndexer::new("hash", None)?;
    let mut manifest = SemanticManifest::default();

    // First call: process 1 of 3 conversations.
    let first = indexer.run_backfill_from_storage_with_sink(
        &storage,
        &data_dir,
        &mut manifest,
        SemanticBackfillStoragePlan {
            tier: TierKind::Fast,
            db_fingerprint: db_fingerprint.clone(),
            model_revision: "hash".to_string(),
            max_conversations: 1,
        },
        &SemanticProgressSink::disabled(),
    )?;
    assert!(!first.published);
    assert_eq!(first.embedded_docs, 1);
    let first_last_message_id = manifest
        .checkpoint
        .as_ref()
        .and_then(|c| c.last_message_id)
        .expect("first checkpoint must persist last_message_id");

    // Second call (simulating a fresh-process resume): also pull in
    // the next 1 conversation. The first conversation must NOT be
    // re-embedded — only one new doc should be produced.
    let second = indexer.run_backfill_from_storage_with_sink(
        &storage,
        &data_dir,
        &mut manifest,
        SemanticBackfillStoragePlan {
            tier: TierKind::Fast,
            db_fingerprint: db_fingerprint.clone(),
            model_revision: "hash".to_string(),
            max_conversations: 1,
        },
        &SemanticProgressSink::disabled(),
    )?;
    assert!(!second.published, "still expect 1 pending conversation");
    assert_eq!(
        second.embedded_docs, 1,
        "resume must embed exactly the new conversation, not re-embed earlier work"
    );
    let second_last_message_id = manifest
        .checkpoint
        .as_ref()
        .and_then(|c| c.last_message_id)
        .expect("second checkpoint must persist last_message_id");
    assert!(
        second_last_message_id > first_last_message_id,
        "last_message_id must strictly advance across resumes; got {first_last_message_id} → {second_last_message_id}"
    );

    // Third call: drives publication.
    let third = indexer.run_backfill_from_storage_with_sink(
        &storage,
        &data_dir,
        &mut manifest,
        SemanticBackfillStoragePlan {
            tier: TierKind::Fast,
            db_fingerprint,
            model_revision: "hash".to_string(),
            max_conversations: 1,
        },
        &SemanticProgressSink::disabled(),
    )?;
    assert!(third.published, "expected publication after third resume");
    assert_eq!(third.embedded_docs, 1);

    Ok(())
}

#[test]
fn forward_compat_fallback_for_v1_shape_manifest_without_last_message_id() -> TestResult {
    // Simulate a pre-#257 binary having written a v1-shape manifest
    // with an active conversation-offset-only checkpoint. The current
    // binary must:
    // 1. Load the manifest without error.
    // 2. Continue resume from `last_offset` (conversation cursor).
    // 3. On the next checkpoint save, the new manifest written to
    //    disk MUST carry `last_message_id`.
    let workdir = TempDir::new()?;
    let data_dir = workdir.path().join("data");
    let vector_dir = data_dir.join("vector_index");
    fs::create_dir_all(&vector_dir)?;
    let db_path = workdir.path().join("agent_search.db");
    let storage = seed_three_conversations(&db_path)?;
    let db_fingerprint = current_db_fingerprint(&db_path)?;

    // Hand-craft a v1 manifest with the conversation cursor set to
    // 1 (i.e. conversation "a" is already embedded; next batch should
    // pick up "b" onwards).
    let v1_manifest = json!({
        "manifest_version": 1,
        "fast_tier": null,
        "quality_tier": null,
        "hnsw": null,
        "backlog": {
            "total_conversations": 3,
            "fast_tier_processed": 1,
            "quality_tier_processed": 0,
            "db_fingerprint": db_fingerprint,
            "computed_at_ms": 1_700_000_000_000_i64
        },
        "checkpoint": {
            "tier": "fast",
            "embedder_id": "fnv1a-384",
            "last_offset": 1,
            "docs_embedded": 1,
            "conversations_processed": 1,
            "total_conversations": 3,
            "db_fingerprint": db_fingerprint,
            "schema_version": coding_agent_search::search::policy::SEMANTIC_SCHEMA_VERSION,
            "chunking_version": coding_agent_search::search::policy::CHUNKING_STRATEGY_VERSION,
            "saved_at_ms": 1_700_000_005_000_i64
        },
        "updated_at_ms": 1_700_000_005_000_i64
    });
    fs::write(
        SemanticManifest::path(&data_dir),
        serde_json::to_string_pretty(&v1_manifest)?,
    )?;

    // Loads cleanly — no UnsupportedVersion error.
    let mut manifest = SemanticManifest::load(&data_dir)?.expect("v1 manifest should load");
    assert_eq!(manifest.manifest_version, 1, "loaded the v1 shape");
    assert!(
        manifest
            .checkpoint
            .as_ref()
            .is_some_and(|c| c.last_message_id.is_none()),
        "v1 checkpoint must deserialize with last_message_id = None"
    );

    // Run one batch — only 2 conversations should remain, and the
    // resume cursor should be the conversation offset (since
    // last_message_id is None on the in-memory checkpoint).
    let indexer = SemanticIndexer::new("hash", None)?;
    let outcome = indexer.run_backfill_from_storage_with_sink(
        &storage,
        &data_dir,
        &mut manifest,
        SemanticBackfillStoragePlan {
            tier: TierKind::Fast,
            db_fingerprint,
            model_revision: "hash".to_string(),
            max_conversations: 1,
        },
        &SemanticProgressSink::disabled(),
    )?;
    assert!(!outcome.published, "still 1 conversation pending");
    assert_eq!(
        outcome.embedded_docs, 1,
        "resume from v1 manifest must embed exactly one new conversation"
    );

    // After the new checkpoint save, the on-disk manifest must
    // include `last_message_id` (proving forward-migration). The
    // manifest_version itself bumped to v2 on save.
    let updated_raw = fs::read_to_string(SemanticManifest::path(&data_dir))?;
    let updated: Value = serde_json::from_str(&updated_raw)?;
    let new_last_message_id = updated
        .get("checkpoint")
        .and_then(|c| c.get("last_message_id"))
        .and_then(Value::as_i64);
    assert!(
        new_last_message_id.is_some(),
        "fresh checkpoint must persist last_message_id; got manifest {updated:#?}"
    );

    Ok(())
}
