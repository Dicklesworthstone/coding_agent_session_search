//! Upgrade an established, uncertified native tier through real maintenance.
//!
//! "Legacy" here means a current-contract mutable artifact with no coverage
//! certificate. These fixtures use canonical storage and the real hash embedder;
//! no model or vector inference is substituted with a test double.

use super::*;
use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use crate::search::semantic_coverage::SemanticParentReader;

const REQUESTED_EMBEDDER: &str = "hash";
const EMBEDDER_ID: &str = "fnv1a-384";
const FOREIGN_EMBEDDER_ID: &str = "minilm-384";

struct LegacyTier {
    directory: tempfile::TempDir,
    storage: Mutex<FrankenStorage>,
    agent_id: i64,
    next_conversation: i64,
}

impl LegacyTier {
    fn new() -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let storage = FrankenStorage::open(&directory.path().join("archive.db"))?;
        let agent_id = storage.ensure_agent(&Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: None,
            kind: AgentKind::Cli,
        })?;
        let mut fixture = Self {
            directory,
            storage: Mutex::new(storage),
            agent_id,
            next_conversation: 0,
        };
        fixture.append_conversation()?;
        fixture.publish_legacy_artifact()?;
        Ok(fixture)
    }

    fn data_dir(&self) -> &Path {
        self.directory.path()
    }

    fn archive(&self) -> Result<std::sync::MutexGuard<'_, FrankenStorage>> {
        self.storage
            .lock()
            .map_err(|error| anyhow::anyhow!("locking legacy semantic fixture: {error}"))
    }

    fn builder_path(&self) -> PathBuf {
        vector_index_path(self.data_dir(), EMBEDDER_ID)
    }

    fn append_conversation(&mut self) -> Result<()> {
        let number = self.next_conversation;
        let conversation = Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: None,
            external_id: Some(format!("legacy-upgrade-{number}")),
            title: Some(format!("Semantic upgrade conversation {number}")),
            source_path: self.data_dir().join(format!("source-{number}.jsonl")),
            started_at: Some(1000 + number),
            ended_at: None,
            approx_tokens: None,
            metadata_json: serde_json::Value::Null,
            messages: vec![Message {
                id: None,
                idx: 0,
                role: MessageRole::User,
                author: None,
                created_at: Some(1000 + number),
                content: format!(
                    "Explain correct semantic coverage for archived conversation {number}."
                ),
                extra_json: serde_json::Value::Null,
                snippets: Vec::new(),
            }],
            source_id: LOCAL_SOURCE_ID.to_owned(),
            origin_host: None,
        };
        {
            let storage = self.archive()?;
            storage.insert_conversation_tree_with_analytics(
                self.agent_id,
                None,
                &conversation,
                true,
            )?;
        }
        self.next_conversation += 1;
        Ok(())
    }

    fn publish_legacy_artifact(&self) -> Result<()> {
        let storage = self.archive()?;
        let indexer = SemanticIndexer::new(REQUESTED_EMBEDDER, Some(self.data_dir()))?;
        let inputs = CanonicalSemanticDocuments::from_storage(&storage)?.into_inputs();
        let vectors = indexer.embed_messages(&inputs)?;
        let index = indexer.build_and_save_index(vectors, self.data_dir())?;
        let count = u64::try_from(index.record_count())?;
        assert_eq!(count, 1);
        drop(index);
        publish_direct_semantic_artifact(
            &storage,
            self.data_dir(),
            &self.builder_path(),
            indexer.embedder_id(),
            indexer.embedder_dimension(),
            count,
            1,
        )?;
        let manifest = SemanticManifest::load_or_default(self.data_dir())?;
        assert!(manifest.coverage.is_empty());
        assert!(manifest.checkpoint.is_none());
        assert!(manifest.fast_tier.as_ref().context("legacy tier")?.ready);
        assert!(semantic_index_has_current_contract(
            self.data_dir(),
            REQUESTED_EMBEDDER
        ));
        Ok(())
    }

    fn assert_complete_generation(&self, expected_messages: u64) -> Result<SemanticManifest> {
        let manifest = SemanticManifest::load_or_default(self.data_dir())?;
        let artifact = manifest.fast_tier.as_ref().context("completed fast tier")?;
        let certificate = manifest
            .coverage
            .get(EMBEDDER_ID)
            .context("completed coverage certificate")?;
        let storage = self.archive()?;
        let state = storage
            .semantic_archive_state()?
            .context("canonical archive authority")?;
        let maximum = storage.max_message_id()?.context("canonical messages")?;
        assert!(artifact.ready);
        assert!(manifest.checkpoint.is_none());
        assert!(certificate.matches_artifact(artifact));
        assert_eq!(certificate.archive_id, state.archive_id);
        assert_eq!(certificate.mutation_seq, state.mutation_seq);
        assert_eq!(certificate.max_message_id, maximum);
        assert_eq!(certificate.doc_count, expected_messages);
        assert_eq!(artifact.conversation_count, expected_messages);
        assert_eq!(certificate.parent_count, expected_messages);
        assert_eq!(
            storage.get_last_embedded_message_id(EMBEDDER_ID)?,
            Some(maximum)
        );
        assert_ne!(
            self.data_dir().join(&artifact.index_path),
            self.builder_path(),
            "the upgraded serving owner must be separate from the mutable builder"
        );
        assert_eq!(
            SemanticParentReader::open(self.data_dir(), certificate)?.count(),
            expected_messages
        );
        let vectors = read_vectors(&self.data_dir().join(&artifact.index_path))?;
        assert_eq!(
            vectors.keys().cloned().collect::<HashSet<_>>(),
            CanonicalSemanticDocuments::from_storage(&storage)?.doc_ids
        );
        assert!(!background_semantic_delta_eligible(
            &storage,
            self.data_dir(),
            REQUESTED_EMBEDDER
        )?);
        Ok(manifest)
    }
}

fn read_vectors(path: &Path) -> Result<BTreeMap<String, Vec<f32>>> {
    let index = FsVectorIndex::open_read_only(path)?;
    assert_eq!(index.wal_record_count(), 0);
    assert_eq!(index.tombstone_count(), 0);
    let mut vectors = BTreeMap::new();
    for record in 0..index.record_count() {
        assert!(
            vectors
                .insert(
                    index.doc_id_at(record)?.to_owned(),
                    index.vector_at_f32(record)?,
                )
                .is_none(),
            "published vector identities must be unique"
        );
    }
    Ok(vectors)
}

#[test]
#[serial_test::serial]
fn gh458_background_upgrade_certifies_legacy_tier_and_reuses_existing_vectors() -> Result<()> {
    let mut fixture = LegacyTier::new()?;
    let retained = read_vectors(&fixture.builder_path())?;
    let initial_maximum = {
        let storage = fixture.archive()?;
        let maximum = storage.max_message_id()?.context("legacy messages")?;
        storage.set_last_embedded_message_id(EMBEDDER_ID, maximum)?;
        storage.set_last_embedded_message_id(FOREIGN_EMBEDDER_ID, maximum)?;
        assert!(!background_semantic_delta_eligible(
            &storage,
            fixture.data_dir(),
            REQUESTED_EMBEDDER
        )?);
        maximum
    };
    fixture.append_conversation()?;
    assert!(background_semantic_delta_eligible(
        &*fixture.archive()?,
        fixture.data_dir(),
        REQUESTED_EMBEDDER
    )?);

    assert_eq!(
        incremental_semantic_embed(REQUESTED_EMBEDDER, fixture.data_dir(), &fixture.storage)?,
        1,
        "the verified legacy vector is reused; only the appended passage is embedded"
    );
    let manifest = fixture.assert_complete_generation(2)?;
    let artifact = manifest.fast_tier.as_ref().context("upgraded fast tier")?;
    let vectors = read_vectors(&fixture.data_dir().join(&artifact.index_path))?;
    for (identity, vector) in retained {
        assert_eq!(vectors.get(&identity), Some(&vector));
    }
    assert_eq!(
        fixture
            .archive()?
            .get_last_embedded_message_id(FOREIGN_EMBEDDER_ID)?,
        Some(initial_maximum),
        "upgrading hash coverage must not certify a different embedder"
    );

    let manifest_bytes = fs::read(SemanticManifest::path(fixture.data_dir()))?;
    assert_eq!(
        finalize_one_shot_semantic_delta(
            REQUESTED_EMBEDDER,
            fixture.data_dir(),
            &*fixture.archive()?,
            2,
        )?,
        2
    );
    assert_eq!(
        fs::read(SemanticManifest::path(fixture.data_dir()))?,
        manifest_bytes,
        "the background finalizer must retain the newly certified owner"
    );
    Ok(())
}

#[test]
#[serial_test::serial]
fn gh458_background_resumes_unready_legacy_checkpoint_without_a_watermark() -> Result<()> {
    let mut fixture = LegacyTier::new()?;
    let retained = read_vectors(&fixture.builder_path())?;
    fixture.append_conversation()?;
    fixture.append_conversation()?;
    let partial = {
        let storage = fixture.archive()?;
        assert_eq!(storage.get_last_embedded_message_id(EMBEDDER_ID)?, None);
        let indexer = SemanticIndexer::new(REQUESTED_EMBEDDER, Some(fixture.data_dir()))?;
        let mut manifest = SemanticManifest::load_or_default(fixture.data_dir())?;
        let total = count_total_conversations_exact(&storage)?;
        // A real foreground batch stops after one missing conversation. The
        // next background invocation must discover and continue this work.
        indexer.run_backfill_from_storage(
            &storage,
            fixture.data_dir(),
            &mut manifest,
            semantic::SemanticBackfillStoragePlan {
                tier: SemanticTierKind::Fast,
                db_fingerprint: lexical_rebuild_content_fingerprint(&storage, total)?,
                model_revision: semantic_model_revision_for_embedder_id(EMBEDDER_ID),
                max_conversations: 1,
            },
        )?
    };
    assert!(!partial.published);
    assert!(partial.checkpoint_saved);
    assert_eq!(partial.embedded_docs, 1);
    let manifest = SemanticManifest::load_or_default(fixture.data_dir())?;
    let checkpoint = manifest.checkpoint.as_ref().context("saved checkpoint")?;
    assert_eq!(checkpoint.embedder_id, EMBEDDER_ID);
    assert_eq!(checkpoint.docs_embedded, 2);
    assert!(!checkpoint.cursor_exhausted);
    assert!(manifest.coverage.is_empty());
    assert!(!manifest.fast_tier.as_ref().context("legacy tier")?.ready);
    assert!(!semantic_index_has_current_contract(
        fixture.data_dir(),
        REQUESTED_EMBEDDER
    ));
    {
        let storage = fixture.archive()?;
        assert_eq!(storage.get_last_embedded_message_id(EMBEDDER_ID)?, None);
        assert!(background_semantic_delta_eligible(
            &storage,
            fixture.data_dir(),
            REQUESTED_EMBEDDER
        )?);
        assert!(!background_semantic_delta_eligible(
            &storage,
            fixture.data_dir(),
            "minilm"
        )?);
    }

    let manifest_bytes = fs::read(SemanticManifest::path(fixture.data_dir()))?;
    let builder_bytes = fs::read(fixture.builder_path())?;
    let staged_bytes = fs::read(&partial.index_path)?;
    assert_eq!(
        finalize_one_shot_semantic_delta(
            REQUESTED_EMBEDDER,
            fixture.data_dir(),
            &*fixture.archive()?,
            2,
        )?,
        1,
        "an unfinished batch must not publish its two-record staging file"
    );
    assert_eq!(
        fs::read(SemanticManifest::path(fixture.data_dir()))?,
        manifest_bytes
    );
    assert_eq!(fs::read(fixture.builder_path())?, builder_bytes);
    assert_eq!(fs::read(&partial.index_path)?, staged_bytes);
    assert_eq!(
        fixture
            .archive()?
            .get_last_embedded_message_id(EMBEDDER_ID)?,
        None
    );

    assert_eq!(
        incremental_semantic_embed(REQUESTED_EMBEDDER, fixture.data_dir(), &fixture.storage)?,
        1,
        "the saved two-record prefix must survive the next background invocation"
    );
    let complete = fixture.assert_complete_generation(3)?;
    let artifact = complete.fast_tier.as_ref().context("completed fast tier")?;
    let vectors = read_vectors(&fixture.data_dir().join(&artifact.index_path))?;
    for (identity, vector) in retained {
        assert_eq!(vectors.get(&identity), Some(&vector));
    }
    Ok(())
}
