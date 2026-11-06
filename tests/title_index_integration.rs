//! Real canonical-storage and native-FSVI regressions for the independent
//! title channel. Hash vectors make lifecycle tests deterministic and offline;
//! these tests make no claim about neural semantic retrieval quality.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use coding_agent_search::franken_sync::compat::{ConnectionExt, RowExt};
use coding_agent_search::indexer::semantic::{
    EmbeddingInput, SemanticIndexer, maintain_title_index, prepare_title_scope_index,
    title_maintenance_needed,
};
use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use coding_agent_search::search::embedder::Embedder;
use coding_agent_search::search::hash_embedder::HashEmbedder;
use coding_agent_search::search::title_fusion::{
    ConversationScore, TitleFusion, TitleSearchConfig, fuse_title_body,
};
use coding_agent_search::search::title_index::{
    TITLE_REPRESENTATION_VERSION, TitleDocId, TitleIndex, load_title_manifest, title_index_root,
    title_input_hash, title_manifest_path,
};
use coding_agent_search::search::vector_index::vector_index_path;
use coding_agent_search::storage::sqlite::FrankenStorage;
use serde_json::json;

const EMBEDDER: &str = "fnv1a-384";
const GENERIC_BODY: &str =
    "Please continue with the implementation and show the remaining details.";

struct Fixture {
    directory: tempfile::TempDir,
    storage: FrankenStorage,
    agent_id: i64,
    workspace_id: i64,
}

impl Fixture {
    fn new() -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let storage = FrankenStorage::open(&directory.path().join("archive.db"))?;
        let agent_id = storage.ensure_agent(&Agent {
            id: None,
            slug: "shelley".into(),
            name: "Shelley".into(),
            version: None,
            kind: AgentKind::Cli,
        })?;
        let workspace_id = storage.ensure_workspace(&directory.path().join("workspace"), None)?;
        Ok(Self {
            directory,
            storage,
            agent_id,
            workspace_id,
        })
    }

    fn data_dir(&self) -> PathBuf {
        self.directory.path().join("data")
    }

    fn seed(&self, name: &str, title: Option<&str>) -> Result<(i64, Conversation)> {
        let conversation = Conversation {
            id: None,
            agent_slug: "shelley".into(),
            workspace: Some(self.directory.path().join("workspace")),
            external_id: Some(format!("title-fixture:{name}")),
            title: title.map(str::to_owned),
            source_path: self.directory.path().join(format!("{name}.jsonl")),
            started_at: Some(1_000),
            ended_at: Some(1_000),
            approx_tokens: None,
            metadata_json: json!({"source":"shelley", "shelley":{"archived":false}}),
            messages: vec![Message {
                id: None,
                idx: 0,
                role: MessageRole::User,
                author: None,
                created_at: Some(1_000),
                content: GENERIC_BODY.into(),
                extra_json: json!({}),
                snippets: Vec::new(),
            }],
            source_id: "local".into(),
            origin_host: None,
        };
        let inserted = self.storage.insert_conversation_tree(
            self.agent_id,
            Some(self.workspace_id),
            &conversation,
        )?;
        Ok((inserted.conversation_id, conversation))
    }

    fn rename(&self, conversation: &mut Conversation, title: Option<&str>) -> Result<()> {
        conversation.title = title.map(str::to_owned);
        conversation.messages.clear();
        let update = self.storage.insert_conversation_tree(
            self.agent_id,
            Some(self.workspace_id),
            conversation,
        )?;
        assert!(update.inserted_indices.is_empty());
        Ok(())
    }

    fn history(&self) -> Result<Vec<(i64, i64, String)>> {
        Ok(self.storage.raw().query_map_collect(
            "SELECT id, conversation_id, content FROM messages ORDER BY id",
            &[],
            |row| Ok((row.get_typed(0)?, row.get_typed(1)?, row.get_typed(2)?)),
        )?)
    }

    fn body_index(&self, engine: &SemanticIndexer) -> Result<Vec<u8>> {
        let inputs = self
            .history()?
            .into_iter()
            .map(|(id, _, body)| Ok(EmbeddingInput::new(u64::try_from(id)?, body)))
            .collect::<Result<Vec<_>>>()?;
        engine.build_and_save_index(engine.embed_messages(&inputs)?, &self.data_dir())?;
        Ok(fs::read(vector_index_path(&self.data_dir(), EMBEDDER))?)
    }
}

fn title_records(index: &TitleIndex) -> Result<BTreeMap<i64, (TitleDocId, Vec<f32>)>> {
    let mut records = BTreeMap::new();
    for artifact in index.artifacts() {
        for record in 0..artifact.record_count() {
            let identity = TitleDocId::parse(artifact.doc_id_at(record)?)
                .context("native artifact must contain title IDs")?;
            assert!(
                records
                    .insert(
                        identity.conversation_id,
                        (identity, artifact.vector_at_f32(record)?),
                    )
                    .is_none()
            );
        }
    }
    Ok(records)
}

fn title_scores(index: &TitleIndex, query: &str) -> Result<Vec<ConversationScore>> {
    let embedding = HashEmbedder::default().embed_sync(query)?;
    let mut scores = Vec::new();
    for artifact in index.artifacts() {
        for hit in artifact.search_top_k(&embedding, 1_000, None)? {
            let identity = TitleDocId::parse(&hit.doc_id).context("title result identity")?;
            scores.push(ConversationScore {
                conversation_id: identity.conversation_id,
                score: hit.score,
            });
        }
    }
    scores.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then(left.conversation_id.cmp(&right.conversation_id))
    });
    Ok(scores)
}

fn open(data_dir: &Path) -> Result<TitleIndex> {
    TitleIndex::open(data_dir, EMBEDDER)?.context("title channel must be present")
}

#[test]
fn gh540_native_title_artifacts_distinguish_identical_bodies_without_synthetic_history()
-> Result<()> {
    let fixture = Fixture::new()?;
    let (first, _) = fixture.seed(
        "first",
        Some("Rust ownership lifetimes and borrow checking"),
    )?;
    let (second, _) = fixture.seed(
        "second",
        Some("PostgreSQL query planner indexes and execution plans"),
    )?;
    let engine = SemanticIndexer::new("hash", None)?;
    let before_history = fixture.history()?;
    let body_bytes = fixture.body_index(&engine)?;
    assert!(prepare_title_scope_index(&fixture.storage)?);
    assert!(!prepare_title_scope_index(&fixture.storage)?);
    let outcome = maintain_title_index(&engine, &fixture.storage, &fixture.data_dir(), 256)?;
    assert!(outcome.complete);
    assert_eq!(outcome.embedded_titles, 2);
    let titles = open(&fixture.data_dir())?;
    let scores = title_scores(
        &titles,
        "PostgreSQL query planner indexes and execution plans",
    )?;
    assert_eq!(
        scores.first().context("title hits")?.conversation_id,
        second
    );
    let body = [
        ConversationScore {
            conversation_id: first,
            score: 0.0,
        },
        ConversationScore {
            conversation_id: second,
            score: 0.0,
        },
    ];
    for fusion in [TitleFusion::Weighted, TitleFusion::Max, TitleFusion::Rrf] {
        let config = TitleSearchConfig {
            enabled: true,
            fusion,
            title_weight: 0.5,
            ..Default::default()
        };
        assert_eq!(
            fuse_title_body(&body, &scores, config)?
                .first()
                .context("fused hits")?
                .conversation_id,
            second
        );
        let disabled_title = TitleSearchConfig {
            title_weight: 0.0,
            ..config
        };
        assert_eq!(
            fuse_title_body(&body, &scores, disabled_title)?
                .first()
                .context("body hits")?
                .conversation_id,
            first
        );
    }
    assert_eq!(fixture.history()?, before_history);
    assert_eq!(
        fs::read(vector_index_path(&fixture.data_dir(), EMBEDDER))?,
        body_bytes
    );
    for (identity, _) in title_records(&titles)?.values() {
        assert!(before_history.iter().any(|(id, conversation, _)| *id
            == identity.representative_message_id
            && *conversation == identity.conversation_id));
    }
    Ok(())
}

#[test]
fn gh540_title_edit_embeds_one_title_and_preserves_body_other_shards_and_retained_owner()
-> Result<()> {
    let fixture = Fixture::new()?;
    let (first, mut editable) =
        fixture.seed("first", Some("Memory reclamation with hazard pointers"))?;
    let (second, _) = fixture.seed("second", Some("Remote database backup and restore"))?;
    let engine = SemanticIndexer::new("hash", None)?;
    let body_bytes = fixture.body_index(&engine)?;
    prepare_title_scope_index(&fixture.storage)?;
    maintain_title_index(&engine, &fixture.storage, &fixture.data_dir(), 256)?;
    let retained = open(&fixture.data_dir())?;
    let prior_records = title_records(&retained)?;
    let prior_manifest = retained.manifest().clone();
    let before_history = fixture.history()?;
    fixture.rename(
        &mut editable,
        Some("Lock free epoch based memory reclamation"),
    )?;
    let changed = maintain_title_index(&engine, &fixture.storage, &fixture.data_dir(), 1)?;
    assert!(changed.complete);
    assert_eq!(changed.examined_conversations, 1);
    assert_eq!(changed.embedded_titles, 1);
    assert_eq!(changed.rewritten_partitions, 1);
    let updated = open(&fixture.data_dir())?;
    let updated_records = title_records(&updated)?;
    assert_ne!(
        prior_records.get(&first).context("old title")?.0.title_hash,
        updated_records
            .get(&first)
            .context("new title")?
            .0
            .title_hash
    );
    assert_eq!(prior_records.get(&second), updated_records.get(&second));
    let second_partition = coding_agent_search::search::title_index::title_partition(second)?;
    assert_eq!(
        prior_manifest.shards.get(&second_partition),
        updated.manifest().shards.get(&second_partition)
    );
    // A second edit reuses the original pathname slot while the first reader
    // remains alive. Its retained native owner must still refer to old bytes.
    fixture.rename(
        &mut editable,
        Some("Concurrent garbage collection algorithms"),
    )?;
    maintain_title_index(&engine, &fixture.storage, &fixture.data_dir(), 1)?;
    retained.verify_unchanged()?;
    assert_eq!(title_records(&retained)?, prior_records);
    assert_eq!(fixture.history()?, before_history);
    assert_eq!(
        fs::read(vector_index_path(&fixture.data_dir(), EMBEDDER))?,
        body_bytes
    );
    Ok(())
}

#[test]
fn gh540_bounded_bootstrap_catches_title_edits_behind_its_cursor() -> Result<()> {
    let fixture = Fixture::new()?;
    let (first, mut conversation) = fixture.seed("first", Some("Original session topic"))?;
    fixture.seed("second", Some("Durable storage journal"))?;
    fixture.seed("third", Some("Network protocol framing"))?;
    let engine = SemanticIndexer::new("hash", None)?;
    prepare_title_scope_index(&fixture.storage)?;
    let started = maintain_title_index(&engine, &fixture.storage, &fixture.data_dir(), 1)?;
    assert!(!started.complete);
    assert_eq!(started.examined_conversations, 1);
    fixture.rename(
        &mut conversation,
        Some("Revised session topic after bootstrap"),
    )?;
    let mut total_embedded = started.embedded_titles;
    let mut completed = false;
    for _ in 0..8 {
        let step = maintain_title_index(&engine, &fixture.storage, &fixture.data_dir(), 1)?;
        assert!(step.examined_conversations <= 1);
        total_embedded += step.embedded_titles;
        if step.complete {
            completed = true;
            break;
        }
    }
    assert!(
        completed,
        "bounded bootstrap must converge after journal catch-up"
    );
    assert_eq!(total_embedded, 4);
    let titles = open(&fixture.data_dir())?;
    let records = title_records(&titles)?;
    assert_eq!(records.len(), 3);
    assert_eq!(
        Some(records.get(&first).context("revised title")?.0.title_hash),
        title_input_hash(conversation.title.as_deref())
    );
    Ok(())
}

#[test]
fn gh540_same_sequence_tombstones_resume_without_skipping_conversations() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.seed("delete-one", Some("First retained title"))?;
    fixture.seed("delete-two", Some("Second retained title"))?;
    fixture.seed("keep", Some("Surviving conversation title"))?;
    let engine = SemanticIndexer::new("hash", None)?;
    prepare_title_scope_index(&fixture.storage)?;
    maintain_title_index(&engine, &fixture.storage, &fixture.data_dir(), 256)?;
    let pattern = fixture
        .directory
        .path()
        .join("delete-*.jsonl")
        .to_string_lossy()
        .into_owned();
    fixture
        .storage
        .forget_conversations_by_source_glob(&pattern, false)?;
    let first = maintain_title_index(&engine, &fixture.storage, &fixture.data_dir(), 1)?;
    assert!(!first.complete);
    assert_eq!(first.removed_titles, 1);
    let second = maintain_title_index(&engine, &fixture.storage, &fixture.data_dir(), 1)?;
    assert!(second.complete);
    assert_eq!(second.removed_titles, 1);
    assert_eq!(open(&fixture.data_dir())?.manifest().indexed_titles, 1);
    Ok(())
}

#[test]
fn gh540_empty_generic_duplicate_unicode_and_secret_titles_are_bounded() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.seed("empty", None)?;
    fixture.seed("generic", Some("New chat"))?;
    let (duplicate_one, _) = fixture.seed("duplicate-one", Some("Identical conversation title"))?;
    let (duplicate_two, _) = fixture.seed("duplicate-two", Some("Identical conversation title"))?;
    let huge = format!("{} suffix outside bounded scan", "λ".repeat(50_000));
    let (unicode, _) = fixture.seed("unicode", Some(&huge))?;
    let secret = "Investigate token ghp_abcdefghijklmnopqrstuvwxyz1234567890";
    let (redacted, _) = fixture.seed("redacted", Some(secret))?;
    let engine = SemanticIndexer::new("hash", None)?;
    prepare_title_scope_index(&fixture.storage)?;
    maintain_title_index(&engine, &fixture.storage, &fixture.data_dir(), 256)?;
    let titles = open(&fixture.data_dir())?;
    let records = title_records(&titles)?;
    assert_eq!(records.len(), 4);
    assert_eq!(
        records
            .get(&duplicate_one)
            .context("first duplicate")?
            .0
            .title_hash,
        records
            .get(&duplicate_two)
            .context("second duplicate")?
            .0
            .title_hash
    );
    assert_ne!(
        records
            .get(&duplicate_one)
            .context("first duplicate")?
            .0
            .identity_hash,
        records
            .get(&duplicate_two)
            .context("second duplicate")?
            .0
            .identity_hash
    );
    assert_eq!(
        Some(records.get(&unicode).context("Unicode title")?.0.title_hash),
        title_input_hash(Some(&huge))
    );
    assert_eq!(
        Some(
            records
                .get(&redacted)
                .context("redacted title")?
                .0
                .title_hash
        ),
        title_input_hash(Some(secret))
    );
    assert_eq!(fixture.history()?.len(), 6);
    Ok(())
}

#[test]
fn gh540_weights_and_noop_updates_do_not_regenerate_title_artifacts() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.seed("first", Some("Persistent index generations"))?;
    let engine = SemanticIndexer::new("hash", None)?;
    prepare_title_scope_index(&fixture.storage)?;
    maintain_title_index(&engine, &fixture.storage, &fixture.data_dir(), 256)?;
    let before_manifest = fs::read(title_manifest_path(&fixture.data_dir(), EMBEDDER)?)?;
    let titles = open(&fixture.data_dir())?;
    let before_records = title_records(&titles)?;
    let scores = title_scores(&titles, "persistent index generations")?;
    for weight in [0.0, 0.25, 0.5, 1.0] {
        for fusion in [TitleFusion::Weighted, TitleFusion::Max, TitleFusion::Rrf] {
            let config = TitleSearchConfig {
                enabled: true,
                title_weight: weight,
                fusion,
                ..Default::default()
            };
            fuse_title_body(&scores, &scores, config)?;
            assert!(!title_maintenance_needed(
                &fixture.storage,
                &fixture.data_dir(),
                EMBEDDER
            )?);
            let unchanged =
                maintain_title_index(&engine, &fixture.storage, &fixture.data_dir(), 1)?;
            assert!(unchanged.unchanged && !unchanged.published);
            assert_eq!(unchanged.examined_conversations, 0);
            assert_eq!(unchanged.embedded_titles, 0);
        }
    }
    assert_eq!(
        fs::read(title_manifest_path(&fixture.data_dir(), EMBEDDER)?)?,
        before_manifest
    );
    assert_eq!(title_records(&open(&fixture.data_dir())?)?, before_records);
    Ok(())
}

#[test]
fn gh540_representation_change_rebuilds_only_titles() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.seed("first", Some("Versioned title input"))?;
    let engine = SemanticIndexer::new("hash", None)?;
    let body = fixture.body_index(&engine)?;
    prepare_title_scope_index(&fixture.storage)?;
    maintain_title_index(&engine, &fixture.storage, &fixture.data_dir(), 256)?;
    let mut old = load_title_manifest(&fixture.data_dir(), EMBEDDER)?.context("manifest")?;
    old.representation_version = "prior-title-policy".into();
    fs::write(
        title_manifest_path(&fixture.data_dir(), EMBEDDER)?,
        serde_json::to_vec(&old)?,
    )?;
    assert!(TitleIndex::open(&fixture.data_dir(), EMBEDDER).is_err());
    assert!(title_maintenance_needed(
        &fixture.storage,
        &fixture.data_dir(),
        EMBEDDER
    )?);
    let rebuilt = maintain_title_index(&engine, &fixture.storage, &fixture.data_dir(), 256)?;
    assert_eq!(rebuilt.embedded_titles, 1);
    assert_eq!(
        open(&fixture.data_dir())?.manifest().representation_version,
        TITLE_REPRESENTATION_VERSION
    );
    assert_eq!(
        fs::read(vector_index_path(&fixture.data_dir(), EMBEDDER))?,
        body
    );
    Ok(())
}

#[test]
fn gh540_publication_failure_retains_previous_generation_and_retries() -> Result<()> {
    let fixture = Fixture::new()?;
    let (_, mut conversation) = fixture.seed("first", Some("Before failed publication"))?;
    let engine = SemanticIndexer::new("hash", None)?;
    prepare_title_scope_index(&fixture.storage)?;
    maintain_title_index(&engine, &fixture.storage, &fixture.data_dir(), 256)?;
    let before = fs::read(title_manifest_path(&fixture.data_dir(), EMBEDDER)?)?;
    fixture.rename(&mut conversation, Some("After retry succeeds"))?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(title_index_root(&fixture.data_dir(), EMBEDDER)?.join("publish.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock)?;
    assert!(maintain_title_index(&engine, &fixture.storage, &fixture.data_dir(), 1).is_err());
    assert_eq!(
        fs::read(title_manifest_path(&fixture.data_dir(), EMBEDDER)?)?,
        before
    );
    fs2::FileExt::unlock(&lock)?;
    assert_eq!(open(&fixture.data_dir())?.manifest().indexed_titles, 1);
    let retried = maintain_title_index(&engine, &fixture.storage, &fixture.data_dir(), 1)?;
    assert!(retried.complete && retried.published);
    assert_eq!(retried.embedded_titles, 1);
    Ok(())
}
