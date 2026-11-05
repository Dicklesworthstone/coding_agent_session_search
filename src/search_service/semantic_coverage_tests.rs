use super::*;
use coding_agent_search::indexer::semantic::{SemanticBackfillStoragePlan, SemanticIndexer};
use coding_agent_search::indexer::semantic_progress::SemanticProgressSink;
use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use coding_agent_search::search::embedder::Embedder;
use coding_agent_search::search::hash_embedder::HashEmbedder;
use coding_agent_search::search::semantic_coverage::write_semantic_parents;
use coding_agent_search::search::semantic_manifest::{SemanticManifest, TierKind};
use coding_agent_search::storage::sqlite::FrankenStorage;

fn publish(root: &Path) -> Result<SemanticManifest> {
    let storage = FrankenStorage::open(&root.join("agent_search.db"))?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let state = storage
        .semantic_archive_state()?
        .context("fixture archive authority")?;
    let mut manifest = SemanticManifest::load_or_default(root)?;
    let outcome = indexer.run_backfill_from_storage_with_sink(
        &storage,
        root,
        &mut manifest,
        SemanticBackfillStoragePlan {
            tier: TierKind::Fast,
            model_revision: "hash".into(),
            max_conversations: usize::MAX,
            db_fingerprint: format!(
                "service-fixture-{}-{}",
                state.archive_id, state.mutation_seq
            ),
        },
        &SemanticProgressSink::disabled(),
    )?;
    ensure!(outcome.published, "fixture did not publish");
    ensure!(
        manifest.coverage.contains_key(indexer.embedder_id()),
        "fixture lacks certificate"
    );
    Ok(manifest)
}

fn append(root: &Path) -> Result<()> {
    let storage = FrankenStorage::open(&root.join("agent_search.db"))?;
    let agent = storage.ensure_agent(&Agent {
        id: None,
        slug: "codex".into(),
        name: "Codex".into(),
        version: None,
        kind: AgentKind::Cli,
    })?;
    storage.insert_conversation_tree(
        agent,
        None,
        &Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: None,
            external_id: Some("new-service-conversation".into()),
            title: Some("new service title".into()),
            source_path: root.join("new-service-conversation.jsonl"),
            started_at: None,
            ended_at: None,
            approx_tokens: None,
            metadata_json: json!({}),
            source_id: "local".into(),
            origin_host: None,
            messages: vec![Message {
                id: None,
                idx: 0,
                role: MessageRole::Agent,
                author: None,
                created_at: None,
                content: "semanticneedle new service passage".into(),
                extra_json: json!({}),
                snippets: Vec::new(),
            }],
        },
    )?;
    Ok(())
}

fn semantic_page(session: &mut Session, mode: &str) -> Result<Value> {
    let (reply, _) = session.handle(request(json!({"op":"semantic_search", "id":1,
        "query":"semanticneedle", "mode":mode, "limit":10})));
    ensure!(reply.ok, "{reply:?}");
    reply.result.context("service omitted response")
}

#[test]
fn certified_service_keeps_generation_during_ingest_then_adopts_completed_publication() -> Result<()>
{
    let (root, mut session, _) = semantic_service_fixture()?;
    publish(root.path())?;
    let initial = semantic_page(&mut session, "semantic")?;
    assert_eq!(initial["count"], 2);
    assert_eq!(initial["semantic_coverage"]["status"], "complete");
    append(root.path())?;
    let partial = semantic_page(&mut session, "semantic")?;
    assert_eq!(partial["count"], 2);
    assert_eq!(partial["semantic_coverage"]["status"], "partial");
    assert_eq!(
        partial["semantic_coverage"]["generation"],
        initial["semantic_coverage"]["generation"]
    );
    assert_eq!(partial["snapshot"]["semantic"]["successful_loads"], 1);
    publish(root.path())?;
    let current = semantic_page(&mut session, "semantic")?;
    assert_eq!(current["count"], 3);
    assert_eq!(current["semantic_coverage"]["status"], "complete");
    assert_ne!(
        current["semantic_coverage"]["generation"],
        partial["semantic_coverage"]["generation"]
    );
    assert_eq!(current["snapshot"]["semantic"]["successful_loads"], 2);
    Ok(())
}

#[test]
fn certified_service_refresh_binds_parent_proof_when_vector_bytes_are_unchanged() -> Result<()> {
    let (root, mut session, _) = semantic_service_fixture()?;
    let mut manifest = publish(root.path())?;
    let before = semantic_page(&mut session, "semantic")?;
    let expected = manifest.clone();
    let id = HashEmbedder::default().id().to_owned();
    let mut certificate = manifest.coverage[&id].clone();
    let vector_digest = certificate.artifact_sha256.clone();
    let storage = FrankenStorage::open(&root.path().join("agent_search.db"))?;
    storage
        .raw()
        .execute("UPDATE conversations SET title = 'new authenticated service title'")?;
    let parent = write_semantic_parents(storage.raw(), root.path())?;
    assert_ne!(parent.sha256, certificate.parent_sha256);
    certificate.parent_path = parent.path;
    certificate.parent_sha256 = parent.sha256;
    certificate.parent_count = parent.count;
    certificate.published_at_ms += 1;
    let mut record = manifest.fast_tier.clone().context("fixture artifact")?;
    record.completed_at_ms = certificate.published_at_ms;
    // Only the canonical title changed; exact passage vectors remain valid.
    manifest.publish_covered_artifact(record, certificate)?;
    manifest.save_if_unchanged(root.path(), Some(&expected))?;
    drop(storage);
    assert_eq!(manifest.coverage[&id].artifact_sha256, vector_digest);
    let after = semantic_page(&mut session, "semantic")?;
    assert_ne!(
        before["semantic_coverage"]["generation"],
        after["semantic_coverage"]["generation"]
    );
    assert_eq!(after["snapshot"]["semantic"]["successful_loads"], 2);
    assert!(
        after["hits"]
            .as_array()
            .unwrap()
            .iter()
            .all(|hit| hit["title"] == "new authenticated service title")
    );
    Ok(())
}

#[test]
fn certified_service_hybrid_keeps_authenticated_payload_when_lexical_metadata_differs() -> Result<()>
{
    let (root, mut session, _) = semantic_service_fixture()?;
    let storage = FrankenStorage::open(&root.path().join("agent_search.db"))?;
    storage
        .raw()
        .execute("UPDATE messages SET created_at = 1700000000000")?;
    storage
        .raw()
        .execute("UPDATE workspaces SET path = '/authenticated/current-workspace'")?;
    drop(storage);
    publish(root.path())?;
    let (reply, _) = session.handle(request(json!({"op":"semantic_search", "id":1,
        "query":"banana", "mode":"hybrid", "limit":10})));
    ensure!(reply.ok, "{reply:?}");
    let reply = reply.result.context("service response")?;
    assert_eq!(reply["realized_mode"], "hybrid");
    let banana = reply["hits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|hit| hit["message_index"] == 13)
        .context("banana hit")?;
    assert_eq!(banana["title"], "Global retrieval");
    assert_eq!(banana["workspace"], "/authenticated/current-workspace");
    assert!(reply.get("semantic_coverage").is_some());
    Ok(())
}
