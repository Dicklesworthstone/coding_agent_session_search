//! Canonical publication boundaries using real storage and native hash vectors.

use super::*;
use crate::franken_sync::compat::TransactionExt;
use crate::model::types::{Agent, AgentKind, MessageRole};
use crate::search::semantic_coverage::SemanticParentReader;
use std::sync::Mutex;

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
            external_id: Some("coverage-publication".into()),
            title: Some("Coverage proof".into()),
            source_path: root.path().join("source.jsonl"),
            started_at: Some(1000),
            ended_at: None,
            approx_tokens: None,
            metadata_json: serde_json::Value::Null,
            source_id: "local".into(),
            origin_host: None,
            messages: vec![message(0)],
        };
        storage.insert_conversation_tree(agent, None, &conversation)?;
        Ok(Self {
            root,
            storage,
            agent,
            conversation,
        })
    }

    fn append(&mut self) -> Result<()> {
        self.conversation
            .messages
            .push(message(i64::try_from(self.conversation.messages.len())?));
        self.storage
            .insert_conversation_tree(self.agent, None, &self.conversation)?;
        Ok(())
    }

    fn publish(&self, indexer: &SemanticIndexer) -> Result<SemanticBackfillBatchOutcome> {
        let mut manifest = SemanticManifest::load_or_default(self.root.path())?;
        let count = crate::indexer::count_total_conversations_exact(&self.storage)?;
        indexer.run_backfill_from_storage(
            &self.storage,
            self.root.path(),
            &mut manifest,
            SemanticBackfillStoragePlan {
                tier: TierKind::Fast,
                db_fingerprint: crate::indexer::lexical_rebuild_content_fingerprint(
                    &self.storage,
                    count,
                )?,
                model_revision: "hash".into(),
                max_conversations: 64,
            },
        )
    }
}

fn message(idx: i64) -> Message {
    Message {
        id: None,
        idx,
        role: MessageRole::User,
        author: None,
        created_at: Some(1000 + idx),
        content: format!("canonical publication evidence passage number {idx}"),
        extra_json: serde_json::Value::Null,
        snippets: Vec::new(),
    }
}

#[test]
fn complete_backfill_seals_owner_and_caches_its_parent_proof() -> Result<()> {
    let fixture = Fixture::new()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    assert!(fixture.publish(&indexer)?.published);
    let manifest = SemanticManifest::load_or_default(fixture.root.path())?;
    let certificate = &manifest.coverage[indexer.embedder_id()];
    let artifact = manifest.fast_tier.as_ref().context("published fast tier")?;
    assert!(certificate.matches_artifact(artifact));
    assert!(artifact.index_path.starts_with("vector_index/served/"));
    assert_ne!(
        fixture.root.path().join(&artifact.index_path),
        vector_index_path(fixture.root.path(), indexer.embedder_id())
    );
    assert_eq!(certificate.parent_count, 1);
    assert_eq!(
        SemanticParentReader::open(fixture.root.path(), certificate)?.count(),
        1
    );
    assert_eq!(
        fixture
            .storage
            .get_last_embedded_message_id(indexer.embedder_id())?,
        Some(certificate.max_message_id)
    );
    assert!(
        fixture.publish(&indexer)?.unchanged,
        "first completed run must leave a usable skip hint"
    );

    // Missing/corrupt proofs must force repair instead of authorizing an
    // unchanged success from vector and archive stamps alone.
    let parent_path = fixture.root.path().join(&certificate.parent_path);
    let mut file = fs::OpenOptions::new().write(true).open(parent_path)?;
    use std::io::{Seek, SeekFrom, Write};
    file.seek(SeekFrom::Start(0))?;
    file.write_all(b"BROKEN!!")?;
    file.sync_all()?;
    assert!(
        indexer
            .completed_backfill_fingerprint(
                &fixture.storage,
                fixture.root.path(),
                &manifest,
                TierKind::Fast,
                "hash",
            )?
            .is_none()
    );
    Ok(())
}

struct MutatingHash {
    inner: HashEmbedder,
    once: Mutex<Option<Box<dyn FnOnce() -> Result<()> + Send>>>,
}

impl Embedder for MutatingHash {
    fn embed_sync(&self, text: &str) -> crate::search::embedder::EmbedderResult<Vec<f32>> {
        self.inner.embed_sync(text)
    }
    fn embed_batch_sync(
        &self,
        texts: &[&str],
    ) -> crate::search::embedder::EmbedderResult<Vec<Vec<f32>>> {
        if let Some(mutate) = self.once.lock().unwrap().take() {
            mutate().map_err(
                |error| crate::search::embedder::EmbedderError::EmbeddingFailed {
                    model: self.inner.id().into(),
                    source: Box::new(std::io::Error::other(error.to_string())),
                },
            )?;
        }
        self.inner.embed_batch_sync(texts)
    }
    fn dimension(&self) -> usize {
        self.inner.dimension()
    }
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn is_semantic(&self) -> bool {
        self.inner.is_semantic()
    }
    fn category(&self) -> frankensearch::ModelCategory {
        self.inner.category()
    }
    fn tier(&self) -> frankensearch::ModelTier {
        self.inner.tier()
    }
}

#[test]
fn ingest_during_embedding_preserves_owner_and_captured_watermark_then_recovers() -> Result<()> {
    let mut fixture = Fixture::new()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    fixture.publish(&indexer)?;
    let original = SemanticManifest::load_or_default(fixture.root.path())?;
    let original_watermark = fixture
        .storage
        .get_last_embedded_message_id(indexer.embedder_id())?;
    fixture.append()?;
    let mut next = fixture.conversation.clone();
    next.messages.push(message(2));
    let database = fixture.storage.database_path()?;
    let agent = fixture.agent;
    let mut racing = SemanticIndexer::new("hash", None)?;
    racing.exact_reuse = false;
    racing.embedder = Box::new(MutatingHash {
        inner: HashEmbedder::default(),
        once: Mutex::new(Some(Box::new(move || {
            FrankenStorage::open(&database)?.insert_conversation_tree(agent, None, &next)?;
            Ok(())
        }))),
    });
    assert!(fixture.publish(&racing).is_err());
    let after = SemanticManifest::load_or_default(fixture.root.path())?;
    assert_eq!(original.coverage, after.coverage);
    assert_eq!(original.fast_tier, after.fast_tier);
    assert_eq!(
        fixture
            .storage
            .get_last_embedded_message_id(indexer.embedder_id())?,
        original_watermark
    );
    assert!(fixture.publish(&indexer)?.published);
    let recovered = SemanticManifest::load_or_default(fixture.root.path())?;
    assert_eq!(recovered.coverage[indexer.embedder_id()].max_message_id, 3);
    assert_eq!(
        recovered
            .fast_tier
            .as_ref()
            .map(|artifact| artifact.doc_count),
        Some(3)
    );
    Ok(())
}

#[test]
fn uncommitted_and_unproven_builds_cannot_retire_certified_generation() -> Result<()> {
    let fixture = Fixture::new()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    fixture.publish(&indexer)?;
    let original = SemanticManifest::load_or_default(fixture.root.path())?;
    let mut transaction = fixture.storage.raw().transaction()?;
    assert!(SemanticBuildSnapshot::capture(&fixture.storage, indexer.embedder_id()).is_err());
    assert!(fixture.publish(&indexer).is_err());
    transaction.rollback()?;

    let mut manifest = original.clone();
    let error = indexer
        .run_backfill_batch(
            &[EmbeddingInput::new(999, "unproven caller supplied passage")],
            fixture.root.path(),
            &mut manifest,
            SemanticBackfillBatchPlan {
                tier: TierKind::Fast,
                db_fingerprint: "caller-only".into(),
                model_revision: "hash".into(),
                total_conversations: 1,
                conversations_in_batch: 1,
                last_offset: 1,
                cursor_exhausted: true,
            },
        )
        .unwrap_err();
    assert!(error.to_string().contains("without canonical coverage"));
    assert_eq!(
        SemanticManifest::load_or_default(fixture.root.path())?,
        original
    );
    Ok(())
}

#[test]
fn parent_damage_during_embedding_cannot_replace_the_previous_serving_owner() -> Result<()> {
    let mut fixture = Fixture::new()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    fixture.publish(&indexer)?;
    let original = SemanticManifest::load_or_default(fixture.root.path())?;
    let old_parent = fixture
        .root
        .path()
        .join(&original.coverage[indexer.embedder_id()].parent_path);
    let served = fixture.root.path().join("vector_index/served");
    fixture.append()?;
    let mut racing = SemanticIndexer::new("hash", None)?;
    racing.exact_reuse = false;
    racing.embedder = Box::new(MutatingHash {
        inner: HashEmbedder::default(),
        once: Mutex::new(Some(Box::new(move || {
            use std::io::Write;
            let mut damaged = 0;
            for entry in fs::read_dir(served)? {
                let path = entry?.path();
                if path.extension().is_some_and(|value| value == "parents") && path != old_parent {
                    let mut file = fs::OpenOptions::new().write(true).open(path)?;
                    file.write_all(b"BROKEN!!")?;
                    file.sync_all()?;
                    damaged += 1;
                }
            }
            anyhow::ensure!(damaged == 1, "expected one newly prepared parent proof");
            Ok(())
        }))),
    });
    let error = fixture.publish(&racing).unwrap_err();
    assert!(
        format!("{error:#}").contains("authenticate semantic parent proof before publication"),
        "the injected damage must reach the publication authentication check: {error:#}"
    );
    let after = SemanticManifest::load_or_default(fixture.root.path())?;
    assert_eq!(after.coverage, original.coverage);
    assert_eq!(after.fast_tier, original.fast_tier);
    assert_eq!(
        SemanticParentReader::open(
            fixture.root.path(),
            &original.coverage[indexer.embedder_id()],
        )?
        .count(),
        1
    );
    assert_eq!(
        fixture
            .storage
            .get_last_embedded_message_id(indexer.embedder_id())?,
        Some(original.coverage[indexer.embedder_id()].max_message_id)
    );
    Ok(())
}

#[test]
fn missing_checkpoint_vectors_restart_full_coverage_without_resurrecting_the_cursor() -> Result<()>
{
    let fixture = Fixture::new()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let mut second = fixture.conversation.clone();
    second.external_id = Some("coverage-second".into());
    second.source_path = fixture.root.path().join("second.jsonl");
    fixture
        .storage
        .insert_conversation_tree(fixture.agent, None, &second)?;
    let fingerprint = crate::indexer::lexical_rebuild_content_fingerprint(&fixture.storage, 2)?;
    let plan = SemanticBackfillStoragePlan {
        tier: TierKind::Fast,
        db_fingerprint: fingerprint,
        model_revision: "hash".into(),
        max_conversations: 1,
    };
    let mut manifest = SemanticManifest::default();
    let first = indexer.run_backfill_from_storage(
        &fixture.storage,
        fixture.root.path(),
        &mut manifest,
        plan.clone(),
    )?;
    assert!(first.checkpoint_saved && !first.published);
    assert!(
        manifest
            .checkpoint
            .as_ref()
            .is_some_and(|checkpoint| checkpoint.last_offset > 0)
    );
    // Retain the old file as forensic evidence while making its recorded
    // staging pathname unavailable, as after an interrupted publication.
    fs::rename(
        &first.index_path,
        fixture.root.path().join("interrupted-stage.fsvi"),
    )?;
    let mut stale = manifest.clone();
    let expected = manifest.clone();
    manifest
        .checkpoint
        .as_mut()
        .context("checkpoint")?
        .saved_at_ms += 1;
    manifest.save_if_unchanged(fixture.root.path(), Some(&expected))?;
    assert!(
        indexer
            .run_backfill_from_storage(
                &fixture.storage,
                fixture.root.path(),
                &mut stale,
                plan.clone(),
            )
            .is_err()
    );
    assert_eq!(
        SemanticManifest::load_or_default(fixture.root.path())?,
        manifest,
        "missing staging must not erase a later publisher's checkpoint"
    );
    let mut full_plan = plan.clone();
    full_plan.max_conversations = 64;
    let recovered = indexer.run_backfill_from_storage(
        &fixture.storage,
        fixture.root.path(),
        &mut manifest,
        full_plan,
    )?;
    assert!(recovered.published);
    assert_eq!(
        manifest.fast_tier.as_ref().map(|record| record.doc_count),
        Some(2)
    );
    assert_eq!(manifest.coverage[indexer.embedder_id()].parent_count, 2);
    assert!(manifest.checkpoint.is_none());
    Ok(())
}

#[test]
fn same_count_mutation_cannot_inherit_a_completed_cache_boundary() -> Result<()> {
    let fixture = Fixture::new()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    fixture.publish(&indexer)?;
    let manifest = SemanticManifest::load_or_default(fixture.root.path())?;
    let artifact = manifest.fast_tier.as_ref().context("artifact")?;
    let plan = SemanticBackfillStoragePlan {
        tier: TierKind::Fast,
        db_fingerprint: artifact.db_fingerprint.clone(),
        model_revision: "hash".into(),
        max_conversations: 64,
    };
    let (_, mut cache) = indexer
        .completed_backfill_cache(&fixture.storage, fixture.root.path(), &manifest, &plan)?
        .context("completed cache")?;
    let mut source = fixture
        .storage
        .get_source("local")?
        .context("local source")?;
    source.host_label = Some("changed canonical source host".into());
    fixture.storage.upsert_source(&source)?;
    let newer = fixture
        .storage
        .semantic_archive_state()?
        .context("new canonical boundary")?;
    assert_eq!(
        crate::indexer::lexical_rebuild_content_fingerprint(&fixture.storage, 1)?,
        plan.db_fingerprint
    );
    cache.archive = BackfillFilePair::archive(&fixture.storage).context("new physical boundary")?;
    let cache_path = fixture
        .root
        .path()
        .join(VECTOR_INDEX_DIR)
        .join(".completed-backfill-fast-fnv1a-384.json");
    persist_completed_backfill_cache(fixture.root.path(), &cache_path, &cache);
    assert!(
        indexer
            .completed_backfill_cache(&fixture.storage, fixture.root.path(), &manifest, &plan)?
            .is_none()
    );
    // No publication authority or watermark was advanced by the skip hint.
    assert!(manifest.coverage[indexer.embedder_id()].mutation_seq < newer.mutation_seq);
    Ok(())
}
