//! Exercise the production worker, native FSVI, canonical database and reader.

use super::*;
use crate::daemon::worker::embedding_source::EmbeddingMessageSource;
use crate::daemon::worker::{EmbeddingJobConfig, EmbeddingWorker, WorkerOperationOutcome};
use crate::franken_sync::compat::{ConnectionExt, ParamValue};
use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use crate::search::model_manager::load_hash_semantic_context_strict;
use crate::search::query::{
    FieldMask, SearchClient, SearchClientOptions, SearchFilters, SemanticTierMode,
};
use crate::search::semantic_manifest::{
    BuildCheckpoint, SemanticShardManifest, SemanticShardRecord,
};

struct Fixture {
    root: tempfile::TempDir,
    storage: FrankenStorage,
    agent: i64,
    conversation: Conversation,
}

impl Fixture {
    fn new() -> Result<Self> {
        let root = tempfile::tempdir()?;
        let storage = FrankenStorage::open(&root.path().join("agent_search.db"))?;
        let agent = storage.ensure_agent(&Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: None,
            kind: AgentKind::Cli,
        })?;
        let conversation = Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: None,
            external_id: Some("daemon-publication".into()),
            title: Some("Daemon publication fixture".into()),
            source_path: root.path().join("history.jsonl"),
            started_at: Some(1_700_000_000_000),
            ended_at: None,
            approx_tokens: None,
            metadata_json: serde_json::Value::Null,
            source_id: "local".into(),
            origin_host: None,
            messages: vec![message(0, "initial canonical compiler history")],
        };
        storage.insert_conversation_tree(agent, None, &conversation)?;
        Ok(Self {
            root,
            storage,
            agent,
            conversation,
        })
    }

    fn db(&self) -> std::path::PathBuf {
        self.root.path().join("agent_search.db")
    }

    fn mutable(&self) -> std::path::PathBuf {
        vector_index_path(self.root.path(), "fnv1a-384")
    }

    fn ledger(&self) -> Result<SemanticManifest> {
        SemanticManifest::load(self.root.path())?.context("published daemon ledger")
    }

    fn admission(&self) -> Result<PublicationAdmission> {
        PublicationAdmission::capture(self.root.path(), &WorkerEmbedderKind::Hash, TierKind::Fast)
    }

    fn run(&self) -> Result<()> {
        let (mut worker, _handle) = EmbeddingWorker::new();
        worker.set_data_dir(self.root.path().to_path_buf());
        let config = EmbeddingJobConfig {
            db_path: self.db().to_string_lossy().into_owned(),
            index_path: self.root.path().to_string_lossy().into_owned(),
            two_tier: false,
            fast_model: None,
            quality_model: Some("hash".into()),
        };
        assert_eq!(
            worker.process_job(&config)?,
            WorkerOperationOutcome::Completed
        );
        let jobs = self.storage.get_embedding_jobs(&config.db_path)?;
        let latest = jobs.first().context("latest daemon job")?;
        assert_eq!(latest.status, "completed", "{:?}", latest.error_message);
        let canonical_count = i64::try_from(
            crate::daemon::worker::SqliteEmbeddingSource::open(&self.db())?.total_docs(),
        )?;
        assert_eq!(
            (latest.completed_docs, latest.total_docs),
            (canonical_count, canonical_count),
        );
        Ok(())
    }

    fn append(&mut self, text: &str) -> Result<()> {
        self.conversation.messages.push(message(
            i64::try_from(self.conversation.messages.len())?,
            text,
        ));
        self.storage
            .insert_conversation_tree(self.agent, None, &self.conversation)?;
        Ok(())
    }

    fn hits(&self, query: &str) -> Result<Vec<crate::search::query::SearchHit>> {
        let setup = load_hash_semantic_context_strict(self.root.path(), &self.db());
        let context = setup
            .context
            .with_context(|| setup.availability.summary())?;
        let client = SearchClient::open_semantic(
            &self.db(),
            SearchClientOptions {
                enable_reload: false,
                enable_warm: false,
                strict_read_only: true,
            },
        )?
        .context("canonical search client")?;
        client.set_semantic_artifacts_context(
            context.embedder,
            context.artifacts,
            context.quality_artifact,
            context.filter_maps,
            context.roles,
        )?;
        let (hits, stats, coverage) = client.search_semantic_with_tier_and_coverage(
            query,
            SearchFilters::default(),
            20,
            0,
            FieldMask::FULL,
            false,
            SemanticTierMode::Single,
        )?;
        assert!(stats.is_none());
        assert_eq!(
            coverage.context("certified daemon coverage")?.status,
            "complete"
        );
        Ok(hits)
    }

    /// Build through the existing production generator while deliberately
    /// omitting publication, as a crash before the serving ledger would do.
    fn generate_only(&self) -> Result<()> {
        let (worker, _handle) = EmbeddingWorker::new();
        let source = crate::daemon::worker::SqliteEmbeddingSource::open(&self.db())?;
        let job = self.storage.upsert_embedding_job(
            &self.db().to_string_lossy(),
            "hash",
            i64::try_from(source.total_docs())?,
        )?;
        self.storage.start_embedding_job(job)?;
        assert_eq!(
            worker.generate_embeddings_from_source(
                &self.storage,
                &source,
                "hash",
                false,
                job,
                self.root.path(),
                &self.db(),
                TierKind::Fast,
                &mut |_| {},
            )?,
            crate::daemon::worker::EmbeddingPassOutcome::Completed,
        );
        Ok(())
    }
}

fn message(idx: i64, content: &str) -> Message {
    Message {
        id: None,
        idx,
        role: MessageRole::Agent,
        author: None,
        created_at: Some(1_700_000_000_000 + idx),
        content: content.into(),
        extra_json: serde_json::Value::Null,
        snippets: Vec::new(),
    }
}

#[test]
fn completed_daemon_job_publishes_the_generation_ordinary_search_reads() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.run()?;
    let manifest = fixture.ledger()?;
    let record = manifest.fast_tier.as_ref().context("fast tier")?;
    let certificate = manifest.coverage.get("fnv1a-384").context("coverage")?;
    assert!(certificate.matches_artifact(record));
    assert_eq!(certificate.doc_count, 1);
    assert!(record.index_path.starts_with("vector_index/served/"));
    assert_eq!(
        fixture.storage.get_last_embedded_message_id("fnv1a-384")?,
        fixture.storage.max_message_id()?,
    );
    let hits = fixture.hits("canonical compiler history")?;
    assert_eq!(hits.len(), 1);
    assert!(
        hits[0]
            .content
            .contains("initial canonical compiler history")
    );
    Ok(())
}

#[test]
fn daemon_role_projection_matches_canonical_packet_normalization() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture
        .storage
        .raw()
        .execute("UPDATE messages SET role = ' AGENT '")?;
    fixture.run()?;
    let index = VectorIndex::open_read_only(&fixture.mutable())?;
    let identity = crate::search::vector_index::parse_semantic_doc_id(index.doc_id_at(0)?)
        .context("canonical document identity")?;
    assert_eq!(identity.role, crate::search::vector_index::ROLE_ASSISTANT);
    assert_eq!(fixture.hits("canonical compiler history")?.len(), 1);
    Ok(())
}

#[test]
fn empty_first_job_finishes_without_loading_a_model_or_inventing_a_ready_artifact() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.storage.raw().execute("DELETE FROM messages")?;
    fixture.run()?;
    assert!(!fixture.mutable().exists());
    let manifest = fixture.ledger()?;
    assert!(manifest.fast_tier.is_none());
    assert!(manifest.coverage.is_empty());
    assert_eq!(
        fixture.storage.get_last_embedded_message_id("fnv1a-384")?,
        Some(0)
    );
    Ok(())
}

#[test]
fn unchanged_vectors_repair_stale_ledger_and_preserve_foreign_checkpoint() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.run()?;
    let before = fs::read(fixture.mutable())?;
    let mut manifest = fixture.ledger()?;
    manifest.coverage.remove("fnv1a-384");
    let artifact = manifest.fast_tier.as_mut().context("fast tier")?;
    artifact.index_path = "vector_index/index-fnv1a-384.fsvi".into();
    artifact.db_fingerprint = "content-v1:999:999:999".into();
    artifact.ready = false;
    let mut sibling = artifact.clone();
    sibling.tier = TierKind::Quality;
    sibling.embedder_id = "minilm-384".into();
    sibling.model_revision = "unexecuted sibling metadata fixture".into();
    manifest.quality_tier = Some(sibling.clone());
    let checkpoint = BuildCheckpoint {
        tier: TierKind::Fast,
        embedder_id: "minilm-384".into(),
        last_offset: 0,
        docs_embedded: 0,
        conversations_processed: 0,
        total_conversations: 1,
        db_fingerprint: "foreign checkpoint boundary".into(),
        schema_version: SEMANTIC_SCHEMA_VERSION,
        chunking_version: CHUNKING_STRATEGY_VERSION,
        saved_at_ms: 1_700_000_000_000,
        last_message_id: None,
        cursor_exhausted: false,
    };
    manifest.checkpoint = Some(checkpoint.clone());
    manifest.save(fixture.root.path())?;
    assert!(
        load_hash_semantic_context_strict(fixture.root.path(), &fixture.db())
            .context
            .is_none()
    );

    fixture.run()?;
    assert_eq!(
        fs::read(fixture.mutable())?,
        before,
        "no-op must retain the native vector file"
    );
    let repaired = fixture.ledger()?;
    assert_eq!(repaired.checkpoint, Some(checkpoint));
    assert_eq!(repaired.quality_tier, Some(sibling));
    assert!(repaired.coverage.contains_key("fnv1a-384"));
    assert_eq!(fixture.hits("canonical compiler history")?.len(), 1);
    Ok(())
}

#[test]
fn daemon_append_promotes_new_coverage_and_acknowledges_only_its_identity_family() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.run()?;
    let original = fixture.ledger()?.coverage["fnv1a-384"].clone();
    fixture.append("newly appended daemon compiler evidence")?;
    fixture
        .storage
        .mark_semantic_identity_rebuild_required(SemanticIdentityTier::Fast)?;
    fixture
        .storage
        .mark_semantic_identity_rebuild_required(SemanticIdentityTier::Quality)?;
    fixture.run()?;
    let updated = fixture.ledger()?.coverage["fnv1a-384"].clone();
    assert_ne!(updated.artifact_sha256, original.artifact_sha256);
    assert!(updated.max_message_id > original.max_message_id);
    assert_eq!(updated.doc_count, 2);
    assert!(
        !fixture
            .storage
            .semantic_identity_rebuild_required(SemanticIdentityTier::Fast)?
    );
    assert!(
        fixture
            .storage
            .semantic_identity_rebuild_required(SemanticIdentityTier::Quality)?
    );
    let hits = fixture.hits("newly appended daemon compiler evidence")?;
    assert!(hits.iter().any(|hit| {
        hit.content
            .contains("newly appended daemon compiler evidence")
    }));
    assert!(
        hits.iter()
            .any(|hit| hit.content.contains("initial canonical compiler history"))
    );
    Ok(())
}

#[test]
fn completed_mutable_generation_recovers_after_publication_failure_without_reembedding()
-> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.run()?;
    let original = fs::read(SemanticManifest::path(fixture.root.path()))?;
    fixture.append("generation survives a failed serving ledger write")?;
    fixture.generate_only()?;
    let mutable = fs::read(fixture.mutable())?;
    assert_eq!(
        fs::read(SemanticManifest::path(fixture.root.path()))?,
        original
    );
    fixture.run()?;
    assert_eq!(fs::read(fixture.mutable())?, mutable);
    assert_eq!(fixture.ledger()?.coverage["fnv1a-384"].doc_count, 2);
    assert!(
        fixture
            .hits("generation survives")?
            .iter()
            .any(|hit| hit.content.contains("failed serving ledger"))
    );
    Ok(())
}

#[test]
fn changed_canonical_content_cannot_be_certified_by_equal_counts() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.run()?;
    let admission = fixture.admission()?;
    let before = fs::read(SemanticManifest::path(fixture.root.path()))?;
    // Deliberately bypass the canonical journal: the independent source replay
    // must catch a same-rowid raw edit as well as normal storage mutations.
    fixture.storage.raw().execute_compat(
        "UPDATE messages SET content = ?1",
        &[ParamValue::from(
            "different source bytes with identical row count",
        )],
    )?;
    let error = admission
        .publish(&fixture.storage, fixture.root.path(), &|| false)
        .unwrap_err();
    assert!(
        error.to_string().contains("canonical identities"),
        "{error:#}"
    );
    assert_eq!(
        fs::read(SemanticManifest::path(fixture.root.path()))?,
        before
    );
    Ok(())
}

#[test]
fn concurrent_ledger_publication_wins_and_is_not_overwritten() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.run()?;
    let admission = fixture.admission()?;
    let mut concurrent = fixture.ledger()?;
    concurrent.backlog.total_conversations += 1;
    concurrent.save(fixture.root.path())?;
    let winner = fs::read(SemanticManifest::path(fixture.root.path()))?;
    let error = admission
        .publish(&fixture.storage, fixture.root.path(), &|| false)
        .unwrap_err();
    assert!(
        matches!(
            error.downcast_ref::<crate::search::semantic_manifest::ManifestError>(),
            Some(crate::search::semantic_manifest::ManifestError::ConcurrentUpdate { .. })
        ),
        "{error:#}"
    );
    assert_eq!(
        fs::read(SemanticManifest::path(fixture.root.path()))?,
        winner
    );
    Ok(())
}

#[test]
fn acknowledgement_refuses_changed_authority_when_guarded_writes_return_false() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.run()?;
    let admission = fixture.admission()?;
    let state = fixture
        .storage
        .semantic_archive_state()?
        .context("canonical state")?;
    let maximum = fixture
        .storage
        .max_message_id()?
        .context("message boundary")?;
    fixture.append("later authority must keep its pending identity repair")?;
    fixture
        .storage
        .mark_semantic_identity_rebuild_required(SemanticIdentityTier::Fast)?;
    assert_ne!(
        fixture.storage.semantic_archive_state()?.as_ref(),
        Some(&state)
    );
    let error = admission
        .acknowledge(
            &fixture.storage,
            fixture.root.path(),
            &state,
            maximum,
            &|| false,
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("publication acknowledgement"),
        "{error:#}"
    );
    assert!(
        fixture
            .storage
            .semantic_identity_rebuild_required(SemanticIdentityTier::Fast)?
    );
    Ok(())
}

#[test]
fn cancellation_after_sealing_keeps_old_selected_coverage_and_retry_recovers() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.run()?;
    fixture.append("cancelled publication remains a private complete candidate")?;
    fixture.generate_only()?;
    let before = fs::read(SemanticManifest::path(fixture.root.path()))?;
    let sealed_count = || -> usize {
        fs::read_dir(fixture.root.path().join("vector_index/served"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "fsvi")
            })
            .count()
    };
    assert_eq!(sealed_count(), 1);
    let error = fixture
        .admission()?
        .publish(&fixture.storage, fixture.root.path(), &|| {
            sealed_count() > 1
        })
        .unwrap_err();
    assert!(error.to_string().contains("cancelled"), "{error:#}");
    assert_eq!(sealed_count(), 2, "the real seal must precede cancellation");
    assert_eq!(
        fs::read(SemanticManifest::path(fixture.root.path()))?,
        before
    );
    fixture.run()?;
    assert_eq!(fixture.ledger()?.coverage["fnv1a-384"].doc_count, 2);
    assert_eq!(fixture.hits("canonical publication")?.len(), 2);
    Ok(())
}

#[test]
fn every_completed_monolithic_pass_revokes_old_selected_shards() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.run()?;
    let artifact = fixture.ledger()?.fast_tier.context("fast tier")?;
    let old_shard = SemanticShardRecord {
        tier: TierKind::Fast,
        embedder_id: artifact.embedder_id.clone(),
        model_revision: artifact.model_revision.clone(),
        schema_version: artifact.schema_version,
        chunking_version: artifact.chunking_version,
        dimension: artifact.dimension,
        shard_index: 0,
        shard_count: 1,
        doc_count: artifact.doc_count,
        total_conversations: artifact.conversation_count,
        db_fingerprint: artifact.db_fingerprint.clone(),
        index_path: artifact.index_path.clone(),
        quantization: "f16".into(),
        mmap_ready: true,
        ann_index_path: None,
        ann_size_bytes: 0,
        ann_ready: false,
        size_bytes: artifact.size_bytes,
        started_at_ms: artifact.started_at_ms,
        completed_at_ms: artifact.completed_at_ms,
        ready: true,
    };
    let mut shards = SemanticShardManifest::default();
    shards.shards.push(old_shard);
    shards.save(fixture.root.path())?;
    assert!(
        !fixture
            .storage
            .semantic_identity_rebuild_required(SemanticIdentityTier::Fast)?
    );
    fixture.run()?;
    let shards = SemanticShardManifest::load(fixture.root.path())?.context("shard ledger")?;
    assert!(!shards.shards[0].ready);
    assert_eq!(fixture.hits("compiler history")?.len(), 1);
    Ok(())
}

#[test]
fn malformed_publication_ledger_fails_job_before_mutating_vectors() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.run()?;
    let vectors = fs::read(fixture.mutable())?;
    fs::write(SemanticManifest::path(fixture.root.path()), b"{malformed")?;
    let (mut worker, _handle) = EmbeddingWorker::new();
    worker.set_data_dir(fixture.root.path().to_owned());
    let db = fixture.db().to_string_lossy().into_owned();
    let config = EmbeddingJobConfig {
        db_path: db.clone(),
        index_path: fixture.root.path().to_string_lossy().into_owned(),
        two_tier: false,
        fast_model: None,
        quality_model: Some("hash".into()),
    };
    assert_eq!(
        worker.process_job(&config)?,
        WorkerOperationOutcome::Completed
    );
    let jobs = fixture.storage.get_embedding_jobs(&db)?;
    assert_eq!(jobs[0].status, "failed");
    assert!(
        jobs[0]
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("publication ledger"))
    );
    assert_eq!(fs::read(fixture.mutable())?, vectors);
    assert_eq!(
        fs::read(SemanticManifest::path(fixture.root.path()))?,
        b"{malformed"
    );
    Ok(())
}
