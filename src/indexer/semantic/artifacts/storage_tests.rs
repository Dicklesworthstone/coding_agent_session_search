//! Exercise the public lifecycle boundary with the real canonical archive.
//! Hash vectors are a deterministic control, not a quality-model qualification.

use super::*;
use std::collections::{BTreeMap, BTreeSet};

use crate::indexer::semantic::{
    SemanticBackfillBatchOutcome, SemanticBackfillStoragePlan, SemanticIndexer,
    packet_embedding_inputs_from_storage, semantic_doc_id_for_input,
};
use crate::indexer::semantic_progress::SemanticProgressSink;
use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use crate::search::embedder::Embedder;
use crate::search::hash_embedder::HashEmbedder;
use crate::search::semantic_manifest::TierKind;
use crate::storage::sqlite::FrankenStorage;
use frankensearch::index::{Quantization, VectorIndex};

fn conversation(name: &str) -> Conversation {
    Conversation {
        id: None,
        agent_slug: "codex".into(),
        workspace: None,
        external_id: Some(name.into()),
        title: Some(name.into()),
        source_path: PathBuf::from(format!("/gh490/{name}.jsonl")),
        started_at: Some(1_700_000_000_000),
        ended_at: None,
        approx_tokens: None,
        metadata_json: serde_json::json!({}),
        messages: vec![Message {
            id: None,
            idx: 0,
            role: MessageRole::User,
            author: None,
            created_at: Some(1_700_000_000_500),
            content: format!("compiler recovery evidence for {name}"),
            extra_json: serde_json::json!({}),
            snippets: Vec::new(),
        }],
        source_id: "local".into(),
        origin_host: None,
    }
}

fn archive(data: &Path) -> Result<(FrankenStorage, i64)> {
    let storage = FrankenStorage::open(&data.join("agent_search.db"))?;
    let agent_id = storage.ensure_agent(&Agent {
        id: None,
        slug: "codex".into(),
        name: "Codex".into(),
        version: None,
        kind: AgentKind::Cli,
    })?;
    storage.insert_conversation_tree(agent_id, None, &conversation("first"))?;
    Ok((storage, agent_id))
}

fn run(storage: &FrankenStorage, data: &Path) -> Result<SemanticBackfillBatchOutcome> {
    // Each invocation reloads the durable checkpoint, like separate CLI passes.
    let mut manifest = SemanticManifest::load(data)?.unwrap_or_default();
    SemanticIndexer::new("hash", None)?.run_capped_backfill_from_storage_with_sink(
        storage,
        data,
        &mut manifest,
        SemanticBackfillStoragePlan {
            tier: TierKind::Quality,
            db_fingerprint: crate::indexer::lexical_storage_fingerprint_for_storage(storage)?,
            model_revision: "hash".into(),
            max_conversations: 1,
        },
        &SemanticProgressSink::disabled(),
    )
}

fn vectors(path: &Path) -> Result<BTreeMap<String, Vec<u32>>> {
    let reader = VectorIndex::open_read_only(path)?;
    ensure!(
        reader.wal_record_count() == 0,
        "reconciled fixture must be compacted"
    );
    let mut result = BTreeMap::new();
    for record in 0..reader.record_count() {
        let prior = result.insert(
            reader.doc_id_at(record)?.to_owned(),
            reader
                .vector_at_f32(record)?
                .iter()
                .map(|value| value.to_bits())
                .collect(),
        );
        ensure!(prior.is_none(), "duplicate vector identity");
    }
    Ok(result)
}

fn assert_scratch(data: &Path, checkpoint: Option<&Path>) -> Result<()> {
    let mut stages = BTreeSet::new();
    for entry in fs::read_dir(data.join(VECTOR_INDEX_DIR))? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        assert!(
            !name.starts_with(".backfill-reuse-"),
            "reuse scratch leaked: {name}"
        );
        if name.starts_with(".staging-") {
            stages.insert(entry.path());
        }
    }
    let expected: BTreeSet<PathBuf> = checkpoint.into_iter().map(Path::to_path_buf).collect();
    assert_eq!(
        stages, expected,
        "only the current compacted checkpoint may remain"
    );
    assert!(plan_backfill_artifacts(data)?.candidates.is_empty());
    Ok(())
}

fn abandoned_snapshot(data: &Path, live: &Path) -> Result<(PathBuf, PathBuf)> {
    let root = data.join(VECTOR_INDEX_DIR);
    let staging = root.join(".staging-quality-minilm-384-deadbeef.fsvi");
    fs::copy(live, &staging)?;
    let reuse = root.join(".backfill-reuse-Abandoned123");
    fs::create_dir(&reuse)?;
    fs::copy(live, reuse.join("candidate.fsvi"))?;
    Ok((staging, reuse))
}

#[test]
fn capped_storage_rollover_reuses_vectors_retires_prior_checkpoints_and_preserves_live_until_publish()
-> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let (storage, agent) = archive(data)?;
    let live = run(&storage, data)?;
    assert!(live.published);
    let live_bytes = fs::read(&live.index_path)?;
    let original_vectors = vectors(&live.index_path)?;
    let reader = VectorIndex::open_read_only(&live.index_path)?;
    let query = HashEmbedder::default().embed_sync("compiler recovery evidence")?;
    let old_hits = format!("{:?}", reader.search_top_k(&query, 10, None)?);
    for name in ["second", "third"] {
        storage.insert_conversation_tree(agent, None, &conversation(name))?;
    }
    let first = run(&storage, data)?;
    assert!(first.checkpoint_saved && !first.published);
    assert_eq!(first.embedded_docs, 1, "the live prefix must be reused");
    assert_eq!(vectors(&first.index_path)?.len(), 2);
    assert_scratch(data, Some(&first.index_path))?;
    assert_eq!(fs::read(&live.index_path)?, live_bytes);

    // Both a new conversation and a new turn in an already-covered parent
    // change the real archive fingerprint, reproducing the reporter's rollover.
    let mut first_conversation = conversation("first");
    let mut tail = first_conversation.messages[0].clone();
    tail.idx = 1;
    tail.content = "compiler recovery evidence appended after checkpoint".into();
    first_conversation.messages.push(tail);
    storage.insert_conversation_tree(agent, None, &first_conversation)?;
    storage.insert_conversation_tree(agent, None, &conversation("fourth"))?;
    let (abandoned, reuse) = abandoned_snapshot(data, &live.index_path)?;
    let second = run(&storage, data)?;
    assert!(second.checkpoint_saved && !second.published);
    assert_eq!(
        second.embedded_docs, 1,
        "only the appended turn spends this pass's cap"
    );
    assert_ne!(first.index_path, second.index_path);
    assert!(!first.index_path.exists() && !wal_path_for(&first.index_path).exists());
    assert!(!abandoned.exists() && !reuse.exists());
    assert_scratch(data, Some(&second.index_path))?;
    let saved = SemanticManifest::load(data)?.context("durable manifest")?;
    assert_eq!(saved.checkpoint.as_ref().unwrap().docs_embedded, 3);
    assert_eq!(
        checkpoint_path(data, saved.checkpoint.as_ref().unwrap()),
        second.index_path
    );

    // With no further ingest, the next checkpoint reuses the current pathname.
    let third = run(&storage, data)?;
    assert!(third.checkpoint_saved && !third.published);
    assert_eq!(third.embedded_docs, 1);
    assert_eq!(third.index_path, second.index_path);
    assert_scratch(data, Some(&third.index_path))?;
    assert_eq!(fs::read(&live.index_path)?, live_bytes);
    assert_eq!(
        old_hits,
        format!("{:?}", reader.search_top_k(&query, 10, None)?)
    );
    let reopened = VectorIndex::open_read_only(&live.index_path)?;
    assert_eq!(
        old_hits,
        format!("{:?}", reopened.search_top_k(&query, 10, None)?)
    );
    drop(reopened);
    drop(reader);

    let done = run(&storage, data)?;
    assert!(done.published && !done.checkpoint_saved);
    assert_eq!(done.embedded_docs, 1);
    assert_eq!(done.index_path, live.index_path);
    assert!(!third.index_path.exists() && !wal_path_for(&third.index_path).exists());
    assert_scratch(data, None)?;
    let actual = vectors(&done.index_path)?;
    let expected: BTreeSet<String> = packet_embedding_inputs_from_storage(&storage)?
        .iter()
        .filter_map(semantic_doc_id_for_input)
        .collect();
    assert_eq!(actual.keys().cloned().collect::<BTreeSet<_>>(), expected);
    assert_eq!(actual.len(), 5);
    for (id, bits) in original_vectors {
        assert_eq!(
            actual.get(&id),
            Some(&bits),
            "a retained vector was re-encoded"
        );
    }
    let saved = SemanticManifest::load(data)?.unwrap();
    assert!(saved.checkpoint.is_none());
    let artifact = saved.quality_tier.as_ref().unwrap();
    assert!(artifact.ready);
    assert_eq!(artifact.doc_count, 5);
    assert_eq!(data.join(&artifact.index_path), done.index_path);
    let reader = VectorIndex::open_read_only(&done.index_path)?;
    assert_eq!(reader.search_top_k(&query, 10, None)?.len(), 5);
    Ok(())
}

#[test]
fn rejected_storage_candidate_releases_reuse_scratch_without_changing_checkpoint_or_live()
-> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let (storage, agent) = archive(data)?;
    let live = run(&storage, data)?;
    for name in ["second", "third"] {
        storage.insert_conversation_tree(agent, None, &conversation(name))?;
    }
    let checkpoint = run(&storage, data)?;
    let original = fs::read(&checkpoint.index_path)?;
    let before_live = fs::read(&live.index_path)?;
    let before_manifest = fs::read(SemanticManifest::path(data))?;
    // Same ID/dimension but an incompatible revision: FSVI admission succeeds,
    // then the real storage reconciliation rejects its private reuse snapshot.
    let replacement = data.join("incompatible.fsvi");
    let mut writer = VectorIndex::create_with_revision(
        &replacement,
        "fnv1a-384",
        "incompatible-revision",
        384,
        Quantization::F16,
    )?;
    for id in vectors(&checkpoint.index_path)?.keys() {
        writer.write_record(id, &[0.25_f32; 384])?;
    }
    writer.finish()?;
    fs::copy(&replacement, &checkpoint.index_path)?;
    let rejected = fs::read(&checkpoint.index_path)?;
    let error = run(&storage, data).unwrap_err();
    assert!(format!("{error:#}").contains("incompatible vector space"));
    assert_eq!(fs::read(&checkpoint.index_path)?, rejected);
    assert_eq!(fs::read(&live.index_path)?, before_live);
    assert_eq!(fs::read(SemanticManifest::path(data))?, before_manifest);
    assert_scratch(data, Some(&checkpoint.index_path))?;

    // Restoration is explicit fixture setup, never an implicit cleanup action.
    fs::write(&checkpoint.index_path, &original)?;
    let done = run(&storage, data)?;
    assert!(done.published);
    assert_eq!(done.embedded_docs, 1);
    assert_eq!(vectors(&done.index_path)?.len(), 3);
    assert_scratch(data, None)?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn completed_storage_cache_still_reclaims_interrupted_scratch_before_returning_unchanged()
-> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let (storage, _) = archive(data)?;
    let live = run(&storage, data)?;
    let before_live = fs::read(&live.index_path)?;
    let before_manifest = fs::read(SemanticManifest::path(data))?;
    let (staging, reuse) = abandoned_snapshot(data, &live.index_path)?;
    let outcome = run(&storage, data)?;
    assert!(outcome.published && outcome.unchanged);
    assert_eq!(outcome.embedded_docs, 0);
    assert!(!staging.exists() && !reuse.exists());
    assert_eq!(fs::read(&live.index_path)?, before_live);
    assert_eq!(fs::read(SemanticManifest::path(data))?, before_manifest);
    assert_scratch(data, None)?;
    assert_eq!(vectors(&live.index_path)?.len(), 1);
    Ok(())
}
