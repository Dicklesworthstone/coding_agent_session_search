//! Legacy readers retain canonical authority even when a later rebuild clears
//! the temporary identity-rebuild marker. These fixtures use the production
//! model-manager loader, as the long-lived TUI does.

use super::{
    FieldMask, SearchClient, SearchClientOptions, SearchFilters, SearchHit, SemanticIdentityError,
};
use crate::indexer::semantic::HASH_VECTOR_SPACE_REVISION;
use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use crate::search::canonicalize::{canonicalize_for_embedding, content_hash, embedding_passages};
use crate::search::embedder::{Embedder, EmbedderError, EmbedderResult};
use crate::search::hash_embedder::HashEmbedder;
use crate::search::model_manager::{SemanticContext, load_hash_semantic_context_strict};
use crate::search::semantic_manifest::SemanticManifest;
use crate::search::vector_index::{
    Quantization, SemanticDocId, VectorIndex, role_code_from_str, vector_index_path,
};
use crate::storage::sqlite::{FrankenStorage, SemanticIdentityTier};
use anyhow::{Context, Result, anyhow};
use frankensearch::core::{EmbeddingIdentityBundleV1, IdentityBoundEmbedding};
use serial_test::serial;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const QUERY: &str = "retained semantic ownership needle";
const ORIGINAL: &str = "retained semantic ownership needle before provider reclassification";

struct Fixture {
    root: tempfile::TempDir,
    storage: FrankenStorage,
    agent: i64,
    conversation_id: i64,
    conversation: Conversation,
}

impl Fixture {
    fn new() -> Result<Self> {
        let root = tempfile::tempdir()?;
        let storage = FrankenStorage::open(&root.path().join("agent_search.db"))?;
        let agent = storage.ensure_agent(&Agent {
            id: None,
            slug: "pi_agent".into(),
            name: "Pi Agent".into(),
            version: None,
            kind: AgentKind::Cli,
        })?;
        let conversation = Conversation {
            id: None,
            agent_slug: "pi_agent".into(),
            workspace: None,
            external_id: Some("retained-legacy-omp".into()),
            title: Some("Retained semantic provider identity".into()),
            // A copied OMP config layout is durable ownership evidence. No
            // transcript needs to be discovered or read by the test.
            source_path: root.path().join(
                "home/.omp/profiles/work/agent/sessions/-projects-cass/retained-legacy.jsonl",
            ),
            started_at: Some(1_700_000_000_000),
            ended_at: None,
            approx_tokens: None,
            metadata_json: serde_json::json!({"source": "pi_agent"}),
            messages: vec![Message {
                id: None,
                idx: 0,
                role: MessageRole::User,
                author: None,
                created_at: Some(1_700_000_000_000),
                content: ORIGINAL.into(),
                extra_json: serde_json::json!({}),
                snippets: Vec::new(),
            }],
            source_id: "local".into(),
            origin_host: None,
        };
        let inserted = storage.insert_conversation_tree(agent, None, &conversation)?;
        assert_eq!(inserted.inserted_indices, vec![0]);
        let fixture = Self {
            root,
            storage,
            agent,
            conversation_id: inserted.conversation_id,
            conversation,
        };
        fixture.publish_legacy("initial")?;
        Ok(fixture)
    }

    fn database(&self) -> PathBuf {
        self.root.path().join("agent_search.db")
    }

    fn publish_legacy(&self, generation: &str) -> Result<()> {
        Self::publish_legacy_at(&self.storage, self.root.path(), generation)
    }

    fn publish_legacy_at(storage: &FrankenStorage, root: &Path, generation: &str) -> Result<()> {
        let embedder = HashEmbedder::default();
        let path = vector_index_path(root, embedder.id());
        let directory = path.parent().context("legacy vector directory")?;
        std::fs::create_dir_all(directory)?;
        let staged = directory.join(format!("{generation}.fsvi"));
        let mut writer = VectorIndex::create_with_revision(
            &staged,
            embedder.id(),
            HASH_VECTOR_SPACE_REVISION,
            embedder.dimension(),
            Quantization::F16,
        )?;
        for message in storage.fetch_messages_for_embedding()? {
            for (chunk, passage) in embedding_passages(&message.content).into_iter().enumerate() {
                let canonical = canonicalize_for_embedding(passage);
                let identity = SemanticDocId {
                    message_id: u64::try_from(message.message_id)?,
                    chunk_idx: u8::try_from(chunk)?,
                    agent_id: u32::try_from(message.agent_id)?,
                    workspace_id: u32::try_from(message.workspace_id.unwrap_or(0))?,
                    source_id: message.source_id_hash,
                    role: role_code_from_str(&message.role).context("fixture message role")?,
                    created_at_ms: message.created_at.unwrap_or(0),
                    content_hash: Some(content_hash(&canonical)),
                };
                writer.write_record(
                    &identity.to_doc_id_string(),
                    &embedder.embed_sync(&canonical)?,
                )?;
            }
        }
        writer.finish()?;
        if path.exists() {
            // Keep the old mapped file intact, as an atomic replacement does.
            // Truncating it would test file corruption rather than revocation.
            std::fs::rename(&path, directory.join(format!("before-{generation}.fsvi")))?;
        }
        std::fs::rename(staged, path)?;
        assert!(SemanticManifest::load(root)?.is_none());
        Ok(())
    }

    fn context(&self) -> Result<SemanticContext> {
        let setup = load_hash_semantic_context_strict(self.root.path(), &self.database());
        let context = setup
            .context
            .ok_or_else(|| anyhow!("{}", setup.availability.summary()))?;
        assert!(
            context
                .artifacts
                .iter()
                .all(|artifact| artifact.coverage_certificate().is_none()),
            "this regression must exercise the legacy admission path"
        );
        let authority = self
            .storage
            .semantic_archive_state()?
            .context("fixture canonical semantic authority")?;
        assert!(
            context
                .artifacts
                .iter()
                .all(|artifact| artifact.legacy_authority() == Some(&authority)),
            "the production loader must seal the original authority before attachment"
        );
        Ok(context)
    }

    fn client(&self) -> Result<SearchClient> {
        self.client_with_context(self.context()?)
    }

    fn client_with_context(&self, context: SemanticContext) -> Result<SearchClient> {
        let client = self.unattached_client()?;
        client.set_semantic_artifacts_context(
            context.embedder,
            context.artifacts,
            context.quality_artifact,
            context.filter_maps,
            context.roles,
        )?;
        Ok(client)
    }

    fn unattached_client(&self) -> Result<SearchClient> {
        SearchClient::open_semantic(
            &self.database(),
            SearchClientOptions {
                enable_reload: false,
                enable_warm: false,
                strict_read_only: true,
            },
        )?
        .context("fixture canonical archive")
    }
}

type EmbeddingInterleave = Box<dyn FnOnce() -> Result<()> + Send>;

/// Preserve the real loaded producer's identity and inference while changing
/// the canonical archive after inference starts and before it returns.
struct InterleavedEmbedder {
    inner: Arc<dyn Embedder>,
    interleave: Mutex<Option<EmbeddingInterleave>>,
    calls: AtomicUsize,
}

impl InterleavedEmbedder {
    fn new(inner: Arc<dyn Embedder>, interleave: EmbeddingInterleave) -> Self {
        Self {
            inner,
            interleave: Mutex::new(Some(interleave)),
            calls: AtomicUsize::new(0),
        }
    }

    fn interleave_error(&self, error: impl std::fmt::Display) -> EmbedderError {
        EmbedderError::EmbeddingFailed {
            model: self.id().into(),
            source: Box::new(std::io::Error::other(error.to_string())),
        }
    }

    fn after_embedding(&self) -> EmbedderResult<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let interleave = self
            .interleave
            .lock()
            .map_err(|_| self.interleave_error("embedding interleave lock poisoned"))?
            .take();
        if let Some(interleave) = interleave {
            interleave().map_err(|error| self.interleave_error(error))?;
        }
        Ok(())
    }
}

impl Embedder for InterleavedEmbedder {
    fn embed_sync(&self, text: &str) -> EmbedderResult<Vec<f32>> {
        let vector = self.inner.embed_sync(text)?;
        self.after_embedding()?;
        Ok(vector)
    }

    fn embed_bound_sync(&self, text: &str) -> EmbedderResult<IdentityBoundEmbedding> {
        let response = self.inner.embed_bound_sync(text)?;
        self.after_embedding()?;
        Ok(response)
    }

    fn identity(&self) -> EmbedderResult<&EmbeddingIdentityBundleV1> {
        self.inner.identity()
    }

    fn id(&self) -> &str {
        self.inner.id()
    }

    fn dimension(&self) -> usize {
        self.inner.dimension()
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

fn search(client: &SearchClient, query: &str, agent: &str) -> Result<Vec<SearchHit>> {
    client
        .search_semantic(
            query,
            SearchFilters {
                agents: HashSet::from([agent.to_owned()]),
                ..SearchFilters::default()
            },
            10,
            0,
            FieldMask::FULL,
            false,
        )
        .map(|(hits, _)| hits)
}

fn assert_identity_refusal(client: &SearchClient, query: &str) {
    let error = search(client, query, "pi_agent").expect_err(
        "a retained legacy reader must not transfer old vector identity to current rows",
    );
    assert_identity_invalidated(&error);
}

fn assert_identity_invalidated(error: &anyhow::Error) {
    assert!(
        matches!(
            error.downcast_ref::<SemanticIdentityError>(),
            Some(SemanticIdentityError::SnapshotInvalidated)
        ),
        "expected canonical semantic identity refusal, got {error:#}"
    );
}

#[test]
#[serial]
fn retained_legacy_semantic_identity_stays_revoked_after_reclassification_rebuild() -> Result<()> {
    let fixture = Fixture::new()?;
    let retained = fixture.client()?;
    let before = fixture
        .storage
        .semantic_archive_state()?
        .context("fixture canonical semantic authority")?;
    let original = search(&retained, QUERY, "pi_agent")?;
    assert_eq!(original.len(), 1);
    assert_eq!(original[0].agent, "pi_agent");
    assert_eq!(original[0].conversation_id, Some(fixture.conversation_id));
    assert_eq!(original[0].content, ORIGINAL);

    let reclassified = fixture.storage.reclassify_legacy_omp_conversations()?;
    assert_eq!(reclassified.conversations_reclassified, 1);
    assert!(
        fixture
            .storage
            .semantic_identity_rebuild_required(SemanticIdentityTier::Fast)?
    );
    assert!(fixture.storage.semantic_invalidations_after(&before)?);
    assert!(
        load_hash_semantic_context_strict(fixture.root.path(), &fixture.database())
            .context
            .is_none(),
        "the existing loader gate must reject an identity-stale cold context"
    );
    for query in [QUERY, "uncached ownership request", ""] {
        assert_identity_refusal(&retained, query);
    }

    // Republish vectors with the real current OMP identities, then complete
    // the per-tier marker. Its completion cannot renew the old Pi reader.
    fixture.publish_legacy("reclassified")?;
    for tier in [SemanticIdentityTier::Fast, SemanticIdentityTier::Quality] {
        fixture.storage.complete_semantic_identity_rebuild(tier)?;
        assert!(!fixture.storage.semantic_identity_rebuild_required(tier)?);
    }
    assert!(fixture.storage.semantic_invalidations_after(&before)?);
    for query in [QUERY, "another uncached ownership request", ""] {
        assert_identity_refusal(&retained, query);
    }

    let reloaded = fixture.client()?;
    let current = search(&reloaded, QUERY, "omp")?;
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].agent, "omp");
    assert_eq!(current[0].conversation_id, Some(fixture.conversation_id));
    assert_eq!(current[0].content, ORIGINAL);
    assert!(search(&reloaded, QUERY, "pi_agent")?.is_empty());
    Ok(())
}

#[test]
#[serial]
fn legacy_semantic_load_cannot_be_rebound_after_reclassification_rebuild() -> Result<()> {
    let fixture = Fixture::new()?;
    // The context is admitted now but may wait in a UI worker before it is
    // attached. A later setter cannot grant its old vectors fresh authority.
    let delayed_context = fixture.context()?;
    let before = fixture
        .storage
        .semantic_archive_state()?
        .context("fixture canonical semantic authority")?;
    assert_eq!(
        fixture
            .storage
            .reclassify_legacy_omp_conversations()?
            .conversations_reclassified,
        1
    );
    fixture.publish_legacy("reclassified")?;
    for tier in [SemanticIdentityTier::Fast, SemanticIdentityTier::Quality] {
        fixture.storage.complete_semantic_identity_rebuild(tier)?;
    }
    assert!(fixture.storage.semantic_invalidations_after(&before)?);

    match fixture.client_with_context(delayed_context) {
        Err(error) => assert_identity_invalidated(&error),
        Ok(client) => assert_identity_refusal(&client, QUERY),
    }
    let fresh = search(&fixture.client()?, QUERY, "omp")?;
    assert_eq!(fresh.len(), 1);
    assert_eq!(fresh[0].agent, "omp");
    assert_eq!(fresh[0].conversation_id, Some(fixture.conversation_id));
    Ok(())
}

#[test]
#[serial]
fn legacy_semantic_inflight_reclassification_cannot_retry_or_cache_under_new_authority()
-> Result<()> {
    for reload_during_embedding in [false, true] {
        let fixture = Fixture::new()?;
        assert_eq!(search(&fixture.client()?, QUERY, "pi_agent")?.len(), 1);
        let before = fixture
            .storage
            .semantic_archive_state()?
            .context("fixture canonical semantic authority")?;
        let client = Arc::new(fixture.unattached_client()?);
        let context = fixture.context()?;
        let root = fixture.root.path().to_path_buf();
        let retained_client = Arc::downgrade(&client);
        let completed = Arc::new(AtomicBool::new(false));
        let completed_in_hook = Arc::clone(&completed);
        let wrapped = Arc::new(InterleavedEmbedder::new(
            context.embedder,
            Box::new(move || {
                let database = root.join("agent_search.db");
                let storage = FrankenStorage::open(&database)?;
                assert_eq!(
                    storage
                        .reclassify_legacy_omp_conversations()?
                        .conversations_reclassified,
                    1
                );
                Fixture::publish_legacy_at(&storage, &root, "during-embedding")?;
                for tier in [SemanticIdentityTier::Fast, SemanticIdentityTier::Quality] {
                    storage.complete_semantic_identity_rebuild(tier)?;
                }
                if reload_during_embedding {
                    let setup = load_hash_semantic_context_strict(&root, &database);
                    let current = setup
                        .context
                        .ok_or_else(|| anyhow!("{}", setup.availability.summary()))?;
                    let client = retained_client.upgrade().context("retained query client")?;
                    client.set_semantic_artifacts_context(
                        current.embedder,
                        current.artifacts,
                        current.quality_artifact,
                        current.filter_maps,
                        current.roles,
                    )?;
                }
                completed_in_hook.store(true, Ordering::SeqCst);
                Ok(())
            }),
        ));
        let embedder: Arc<dyn Embedder> = wrapped.clone();
        client.set_semantic_artifacts_context(
            embedder,
            context.artifacts,
            context.quality_artifact,
            context.filter_maps,
            context.roles,
        )?;
        assert!(!completed.load(Ordering::SeqCst));

        assert_identity_refusal(&client, QUERY);
        assert!(
            completed.load(Ordering::SeqCst),
            "the real reclassification, rebuild and optional reload must all complete"
        );
        assert_eq!(wrapped.calls.load(Ordering::SeqCst), 1);
        assert!(fixture.storage.semantic_invalidations_after(&before)?);
        assert!(
            !fixture
                .storage
                .semantic_identity_rebuild_required(SemanticIdentityTier::Fast)?
        );
        {
            let mut state = client
                .semantic
                .lock()
                .map_err(|_| anyhow!("fixture semantic lock poisoned"))?;
            assert!(
                state
                    .as_mut()
                    .context("retained or reloaded semantic state")?
                    .query_cache
                    .get_cached(&canonicalize_for_embedding(QUERY))
                    .is_none(),
                "the invalidated request must neither cache its vector nor retry into a new cache"
            );
        }

        if !reload_during_embedding {
            assert_identity_refusal(&client, QUERY);
            assert_eq!(wrapped.calls.load(Ordering::SeqCst), 1);
            let current = fixture.context()?;
            client.set_semantic_artifacts_context(
                current.embedder,
                current.artifacts,
                current.quality_artifact,
                current.filter_maps,
                current.roles,
            )?;
        }
        let recovered = search(&client, QUERY, "omp")?;
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].agent, "omp");
        assert_eq!(recovered[0].conversation_id, Some(fixture.conversation_id));
        assert_eq!(wrapped.calls.load(Ordering::SeqCst), 1);
    }
    Ok(())
}

#[test]
#[serial]
fn retained_legacy_semantic_identity_allows_unchanged_passages_after_append() -> Result<()> {
    let fixture = Fixture::new()?;
    let retained = fixture.client()?;
    let before = fixture
        .storage
        .semantic_archive_state()?
        .context("fixture canonical semantic authority")?;
    let original = search(&retained, QUERY, "pi_agent")?;
    assert_eq!(original.len(), 1);

    let mut appended = fixture.conversation.clone();
    appended.messages.push(Message {
        id: None,
        idx: 1,
        role: MessageRole::User,
        author: None,
        created_at: Some(1_700_000_001_000),
        content: "newly appended passage is outside the retained legacy vector snapshot".into(),
        extra_json: serde_json::json!({}),
        snippets: Vec::new(),
    });
    let inserted = fixture
        .storage
        .insert_conversation_tree(fixture.agent, None, &appended)?;
    assert_eq!(inserted.conversation_id, fixture.conversation_id);
    assert_eq!(inserted.inserted_indices, vec![1]);
    let after = fixture
        .storage
        .semantic_archive_state()?
        .context("appended canonical semantic authority")?;
    assert_eq!(after.archive_id, before.archive_id);
    assert!(after.mutation_seq > before.mutation_seq);
    assert!(!fixture.storage.semantic_invalidations_after(&before)?);

    let still_current = search(&retained, QUERY, "pi_agent")?;
    assert_eq!(still_current.len(), 1);
    assert_eq!(
        still_current[0].conversation_id,
        original[0].conversation_id
    );
    assert_eq!(still_current[0].agent, "pi_agent");
    assert_eq!(still_current[0].content, ORIGINAL);
    assert_eq!(still_current[0].score, original[0].score);
    Ok(())
}
