//! File-backed no-op proofs through the public recovery lease and real storage.
use super::*;
use crate::indexer::semantic::SemanticIndexer;
use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use crate::search::embedder::Embedder;
use crate::search::hash_embedder::HashEmbedder;
use crate::search::semantic_manifest::HnswRecord;
use crate::search::vector_index::VECTOR_INDEX_DIR;
use std::collections::BTreeMap;

fn conversation(name: &str) -> Conversation {
    Conversation {
        id: None, agent_slug: "codex".into(), workspace: None,
        external_id: Some(name.into()), title: Some(name.into()),
        source_path: PathBuf::from(format!("/unchanged/{name}.jsonl")),
        started_at: Some(1_700_000_000_000), ended_at: None, approx_tokens: None,
        metadata_json: serde_json::json!({}), source_id: "local".into(), origin_host: None,
        messages: vec![Message {
            id: None, idx: 0, role: MessageRole::User, author: None,
            created_at: Some(1_700_000_000_500),
            content: format!("compiler recovery checkpoint {name}"),
            extra_json: serde_json::json!({}), snippets: Vec::new(),
        }],
    }
}

fn setup(data: &Path) -> Result<(FrankenStorage, SemanticManifest, SemanticBackfillStoragePlan, PathBuf)> {
    let storage = FrankenStorage::open(&data.join("agent_search.db"))?;
    let agent = storage.ensure_agent(&Agent {
        id: None, slug: "codex".into(), name: "Codex".into(), version: None, kind: AgentKind::Cli,
    })?;
    for name in ["first", "second", "third"] {
        storage.insert_conversation_tree(agent, None, &conversation(name))?;
    }
    let plan = SemanticBackfillStoragePlan {
        tier: TierKind::Quality,
        db_fingerprint: crate::indexer::lexical_storage_fingerprint_for_storage(&storage)?,
        model_revision: "hash".into(), max_conversations: 16,
    };
    let indexer = SemanticIndexer::new("hash", None)?;
    let mut manifest = SemanticManifest::default();
    let outcome = indexer.run_backfill_from_storage(&storage, data, &mut manifest, plan.clone())?;
    assert!(outcome.published);
    let reader = VectorIndex::open_read_only(&outcome.index_path)?;
    let ann = indexer.build_hnsw_index(&reader, data, None, None)?;
    manifest.hnsw = Some(HnswRecord {
        base_tier: plan.tier, embedder_id: indexer.embedder_id().into(), ef_search: 16,
        index_path: ann.strip_prefix(data)?.to_string_lossy().into_owned(),
        size_bytes: fs::metadata(&ann)?.len(), built_at_ms: manifest.updated_at_ms, ready: true,
    });
    manifest.save(data)?;
    Ok((storage, manifest, plan, outcome.index_path))
}

fn cache_path(data: &Path) -> PathBuf {
    data.join(VECTOR_INDEX_DIR).join(".completed-backfill-quality-fnv1a-384.json")
}

fn snapshot(data: &Path) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![data.join(VECTOR_INDEX_DIR)];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let path = entry?.path();
            if path.is_dir() { pending.push(path); }
            else { files.insert(path.clone(), fs::read(path)?); }
        }
    }
    Ok(files)
}

#[test]
fn absent_corrupt_or_stale_cache_preserves_publication_ann_and_existing_and_fresh_search() -> Result<()> {
    for mode in 0..3 {
        for capped in [false, true] {
            let temp = tempfile::tempdir()?;
            let data = temp.path();
            let (storage, mut manifest, plan, live) = setup(data)?;
            match mode {
                0 => { fs::rename(cache_path(data), data.join("retained-cache.json"))?; }
                1 => { fs::write(cache_path(data), b"{truncated")?; }
                _ => { storage.raw().execute("CREATE TABLE unrelated_metadata (id INTEGER PRIMARY KEY)")?; }
            }
            let indexer = SemanticIndexer::new("hash", None)?;
            assert!(indexer.completed_backfill_fingerprint(&storage, data, &manifest,
                plan.tier, &plan.model_revision)?.is_none(), "fixture must miss the skip cache");
            let before = snapshot(data)?;
            let ledger = manifest.clone();
            let reader = VectorIndex::open_read_only(&live)?;
            let query = HashEmbedder::default().embed_sync("compiler recovery checkpoint")?;
            let hits = reader.search_top_k(&query, 8, None)?;
            let outcome = if capped {
                indexer.run_capped_backfill_from_storage_with_sink(
                    &storage, data, &mut manifest, plan.clone(), &SemanticProgressSink::disabled(),
                )?
            } else {
                indexer.run_backfill_from_storage(&storage, data, &mut manifest, plan.clone())?
            };
            assert!(outcome.unchanged && outcome.published && !outcome.checkpoint_saved);
            assert_eq!(outcome.embedded_docs, 0);
            assert_eq!(outcome.total_conversations, 3);
            assert_eq!(outcome.last_offset, 3);
            assert_eq!(manifest, ledger);
            assert!(manifest.hnsw.as_ref().unwrap().ready);
            assert_eq!(snapshot(data)?, before, "no publication, cache, or graph rewrite");
            assert_eq!(reader.search_top_k(&query, 8, None)?, hits);
            assert_eq!(VectorIndex::open_read_only(&live)?.search_top_k(&query, 8, None)?, hits);
        }
    }
    Ok(())
}

#[test]
fn same_id_content_or_filter_edits_and_deletions_are_not_zero_delta() -> Result<()> {
    for sql in [
        "UPDATE messages SET content = 'replacement content at the same rowid' WHERE id = 1",
        "UPDATE messages SET role = 'tool' WHERE id = 1",
        "DELETE FROM messages WHERE id = 1",
    ] {
        let temp = tempfile::tempdir()?;
        let data = temp.path();
        let (storage, mut manifest, plan, _) = setup(data)?;
        fs::write(cache_path(data), b"{truncated")?;
        storage.raw().execute(sql)?;
        if !sql.starts_with("DELETE") {
            assert_eq!(crate::indexer::lexical_storage_fingerprint_for_storage(&storage)?,
                plan.db_fingerprint, "negative must evade the coarse fingerprint");
        }
        let before = snapshot(data)?;
        let inner = engine::SemanticIndexer::new("hash", None)?;
        assert!(prove_unchanged(&inner, &storage, data, &manifest, &plan,
            &SemanticProgressSink::disabled(), || Ok(()))?.is_none());
        assert_eq!(snapshot(data)?, before, "failed proof itself never mutates the archive");
        let outcome = SemanticIndexer::new("hash", None)?.run_backfill_from_storage(
            &storage, data, &mut manifest, plan,
        )?;
        assert!(!outcome.unchanged && outcome.published);
        assert!(!manifest.hnsw.as_ref().unwrap().ready, "a real delta still revokes the old ANN");
        let expected: HashSet<_> = engine::packet_embedding_inputs_from_storage(&storage)?
            .iter().filter_map(engine::semantic_doc_id_for_input).collect();
        let reader = VectorIndex::open_read_only(&outcome.index_path)?;
        let actual: HashSet<_> = (0..reader.record_count())
            .map(|record| reader.doc_id_at(record).map(str::to_owned))
            .collect::<std::result::Result<_, _>>()?;
        assert_eq!(actual, expected);
    }
    Ok(())
}

#[test]
fn no_op_still_reclaims_interrupted_scratch_without_republishing() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let (storage, mut manifest, plan, live) = setup(data)?;
    fs::write(cache_path(data), b"{truncated")?;
    let before = snapshot(data)?;
    let staging = data.join(VECTOR_INDEX_DIR).join(".staging-quality-minilm-384-deadbeef.fsvi");
    fs::copy(&live, &staging)?;
    let reuse = data.join(VECTOR_INDEX_DIR).join(".backfill-reuse-Killed123");
    fs::create_dir(&reuse)?;
    fs::copy(&live, reuse.join("candidate.fsvi"))?;
    let outcome = SemanticIndexer::new("hash", None)?.run_backfill_from_storage(
        &storage, data, &mut manifest, plan,
    )?;
    assert!(outcome.unchanged);
    assert!(!staging.exists() && !reuse.exists());
    assert_eq!(snapshot(data)?, before);
    Ok(())
}

#[test]
fn missing_cache_never_resurrects_unready_artifacts_or_discards_pending_wal() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let (storage, mut manifest, plan, live) = setup(data)?;
    let inner = engine::SemanticIndexer::new("hash", None)?;
    manifest.quality_tier.as_mut().unwrap().ready = false;
    let before = snapshot(data)?;
    assert!(prove_unchanged(&inner, &storage, data, &manifest, &plan,
        &SemanticProgressSink::disabled(), || Ok(()))?.is_none());
    assert_eq!(snapshot(data)?, before);
    manifest.quality_tier.as_mut().unwrap().ready = true;
    fs::write(wal_path_for(&live), b"unfinished WAL")?;
    let before = snapshot(data)?;
    assert!(prove_unchanged(&inner, &storage, data, &manifest, &plan,
        &SemanticProgressSink::disabled(), || Ok(()))?.is_none());
    assert_eq!(snapshot(data)?, before);
    Ok(())
}

#[test]
fn changed_archive_during_proof_refuses_before_publication_or_readiness_changes() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let (storage, manifest, plan, _) = setup(data)?;
    let before = snapshot(data)?;
    let inner = engine::SemanticIndexer::new("hash", None)?;
    let error = prove_unchanged(&inner, &storage, data, &manifest, &plan,
        &SemanticProgressSink::disabled(), || {
            storage.raw().execute("UPDATE messages SET content = 'changed during proof' WHERE id = 1")?;
            Ok(())
        }).unwrap_err();
    assert!(format!("{error:#}").contains("changed during unchanged proof"));
    assert_eq!(snapshot(data)?, before);
    Ok(())
}
