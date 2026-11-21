//! Production title retrieval over real canonical rows and native FSVI files.
//!
//! The explicit hash control keeps these lifecycle/ranking contracts offline and
//! deterministic. These tests do not attest neural semantic retrieval quality.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use coding_agent_search::franken_sync::compat::{ConnectionExt, RowExt};
use coding_agent_search::indexer::semantic::{
    EmbeddingInput, SemanticIndexer, maintain_title_index, prepare_title_scope_index,
};
use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use coding_agent_search::search::hash_embedder::HashEmbedder;
use coding_agent_search::search::query::{
    FieldMask, MatchType, SearchClient, SearchClientOptions, SearchFilters, SearchHit,
    SemanticSearchOutcome, SemanticTierMode,
};
use coding_agent_search::search::title_fusion::{TitleFusion, TitleSearchConfig};
use coding_agent_search::search::title_index::{title_index_root, title_manifest_path};
use coding_agent_search::search::vector_index::{
    ROLE_ASSISTANT, ROLE_TOOL, SemanticFilterMaps, SemanticIndexArtifact, role_code_from_str,
    vector_index_path,
};
use coding_agent_search::sources::provenance::{Source, SourceFilter};
use coding_agent_search::storage::sqlite::FrankenStorage;
use serde_json::json;

const EMBEDDER: &str = "fnv1a-384";
const BODY: &str = "The implementation is ready for another review. This entry records the discussion and verification details.";
const TOPIC: &str = "PostgreSQL query planner indexes and execution plans";

struct Fixture {
    directory: tempfile::TempDir,
    storage: FrankenStorage,
}

struct Session {
    id: i64,
    agent_id: i64,
    workspace_id: i64,
    conversation: Conversation,
}

impl Fixture {
    fn new() -> Result<Self> {
        let directory = tempfile::tempdir()?;
        fs::create_dir_all(directory.path().join("data"))?;
        let storage = FrankenStorage::open(&directory.path().join("data/archive.db"))?;
        Ok(Self { directory, storage })
    }

    fn data_dir(&self) -> PathBuf {
        self.directory.path().join("data")
    }

    fn db_path(&self) -> PathBuf {
        self.data_dir().join("archive.db")
    }

    fn seed(&self, name: &str, title: &str) -> Result<Session> {
        self.seed_in(
            name,
            title,
            "shelley",
            "project-one",
            "local",
            &[(MessageRole::User, 1_000, BODY)],
        )
    }

    fn seed_in(
        &self,
        name: &str,
        title: &str,
        agent: &str,
        workspace: &str,
        source: &str,
        messages: &[(MessageRole, i64, &str)],
    ) -> Result<Session> {
        let agent_id = self.storage.ensure_agent(&Agent {
            id: None,
            slug: agent.into(),
            name: agent.into(),
            version: None,
            kind: AgentKind::Cli,
        })?;
        let workspace = self.directory.path().join(workspace);
        let workspace_id = self.storage.ensure_workspace(&workspace, None)?;
        let conversation = Conversation {
            id: None,
            agent_slug: agent.into(),
            workspace: Some(workspace),
            external_id: Some(format!("title-search:{name}")),
            title: Some(title.into()),
            source_path: self.directory.path().join(format!("{name}.jsonl")),
            started_at: messages.iter().map(|(_, timestamp, _)| *timestamp).min(),
            ended_at: messages.iter().map(|(_, timestamp, _)| *timestamp).max(),
            approx_tokens: None,
            metadata_json: json!({"source": agent}),
            messages: messages
                .iter()
                .enumerate()
                .map(|(index, (role, timestamp, content))| {
                    Ok(Message {
                        id: None,
                        idx: i64::try_from(index)?,
                        role: role.clone(),
                        author: None,
                        created_at: Some(*timestamp),
                        content: (*content).into(),
                        extra_json: json!({}),
                        snippets: Vec::new(),
                    })
                })
                .collect::<Result<Vec<_>>>()?,
            source_id: source.into(),
            origin_host: (source == "work-laptop").then(|| "work-host".into()),
        };
        let inserted =
            self.storage
                .insert_conversation_tree(agent_id, Some(workspace_id), &conversation)?;
        Ok(Session {
            id: inserted.conversation_id,
            agent_id,
            workspace_id,
            conversation,
        })
    }

    fn build(&self) -> Result<()> {
        let inputs = self.storage.raw().query_map_collect(
            "SELECT m.id, c.agent_id, c.workspace_id, c.source_id, m.role, \
             m.created_at, m.content FROM messages m \
             JOIN conversations c ON c.id = m.conversation_id ORDER BY m.id",
            &[],
            |row| {
                Ok((
                    row.get_typed::<i64>(0)?,
                    row.get_typed::<i64>(1)?,
                    row.get_typed::<i64>(2)?,
                    row.get_typed::<String>(3)?,
                    row.get_typed::<String>(4)?,
                    row.get_typed::<i64>(5)?,
                    row.get_typed::<String>(6)?,
                ))
            },
        )?;
        let inputs = inputs
            .into_iter()
            .map(
                |(message, agent, workspace, source, role, timestamp, content)| {
                    Ok(EmbeddingInput {
                        message_id: u64::try_from(message)?,
                        created_at_ms: timestamp,
                        agent_id: u32::try_from(agent)?,
                        workspace_id: u32::try_from(workspace)?,
                        source_id: crc32fast::hash(source.as_bytes()),
                        role: role_code_from_str(&role).context("fixture message role")?,
                        chunk_idx: 0,
                        content,
                    })
                },
            )
            .collect::<Result<Vec<_>>>()?;
        let engine = SemanticIndexer::new("hash", None)?;
        engine.build_and_save_index(engine.embed_messages(&inputs)?, &self.data_dir())?;
        prepare_title_scope_index(&self.storage)?;
        self.maintain_titles()
    }

    fn maintain_titles(&self) -> Result<()> {
        let outcome = maintain_title_index(
            &SemanticIndexer::new("hash", None)?,
            &self.storage,
            &self.data_dir(),
            256,
        )?;
        assert!(outcome.complete, "fixture title publication must complete");
        Ok(())
    }

    fn client(&self, roles: Option<HashSet<u8>>) -> Result<SearchClient> {
        let client = SearchClient::open_semantic(
            &self.db_path(),
            SearchClientOptions {
                enable_reload: false,
                enable_warm: false,
                strict_read_only: true,
            },
        )?
        .context("canonical semantic client")?;
        client.set_semantic_context(
            Arc::new(HashEmbedder::default()),
            SemanticIndexArtifact::open(vector_index_path(&self.data_dir(), EMBEDDER), None)?,
            None,
            SemanticFilterMaps::from_storage(&self.storage)?,
            roles,
        )?;
        Ok(client)
    }

    fn rename(&self, session: &Session, title: &str) -> Result<()> {
        let mut conversation = session.conversation.clone();
        conversation.title = Some(title.into());
        conversation.messages.clear();
        let changed = self.storage.insert_conversation_tree(
            session.agent_id,
            Some(session.workspace_id),
            &conversation,
        )?;
        assert_eq!(changed.conversation_id, session.id);
        assert!(changed.inserted_indices.is_empty());
        Ok(())
    }

    fn history(&self) -> Result<Vec<(i64, i64, String)>> {
        Ok(self.storage.raw().query_map_collect(
            "SELECT id, conversation_id, content FROM messages ORDER BY id",
            &[],
            |row| Ok((row.get_typed(0)?, row.get_typed(1)?, row.get_typed(2)?)),
        )?)
    }

    fn title_bytes(&self) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
        let root = title_index_root(&self.data_dir(), EMBEDDER)?;
        let mut files = BTreeMap::new();
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                files.insert(entry.path(), fs::read(entry.path())?);
            }
        }
        Ok(files)
    }
}

fn title_config(weight: f32) -> TitleSearchConfig {
    TitleSearchConfig {
        enabled: true,
        title_weight: weight,
        ..Default::default()
    }
}

fn query(
    client: &SearchClient,
    text: &str,
    filters: SearchFilters,
    limit: usize,
    offset: usize,
) -> Result<SemanticSearchOutcome> {
    client.search_semantic_with_metadata(
        text,
        filters,
        limit,
        offset,
        FieldMask::FULL,
        false,
        SemanticTierMode::Single,
    )
}

fn identities(hits: &[SearchHit]) -> Vec<(Option<i64>, String, Option<usize>, u32)> {
    hits.iter()
        .map(|hit| {
            (
                hit.conversation_id,
                hit.source_path.clone(),
                hit.line_number,
                hit.score.to_bits(),
            )
        })
        .collect()
}

#[test]
fn gh540_live_title_channel_distinguishes_identical_bodies_and_preserves_off_policy() -> Result<()>
{
    let fixture = Fixture::new()?;
    let first = fixture.seed("first", "Rust ownership lifetimes and borrow checking")?;
    let second = fixture.seed("second", "Network congestion control algorithms")?;
    let target = fixture.seed("target", TOPIC)?;
    fixture.build()?;
    let history = fixture.history()?;
    let body_before = fs::read(vector_index_path(&fixture.data_dir(), EMBEDDER))?;
    let titles_before = fixture.title_bytes()?;
    let client = fixture.client(None)?;
    let incumbent = query(&client, TOPIC, SearchFilters::default(), 10, 0)?;
    assert!(!incumbent.hits.is_empty());
    assert!(incumbent.titles.is_none());

    // Disabled policy does not open even an invalid title location or change
    // the incumbent's message-level scores, ordering and deduplication.
    client.set_title_search_config(
        &fixture.directory.path().join("absent-title-directory"),
        TitleSearchConfig::default(),
    )?;
    let disabled = query(&client, TOPIC, SearchFilters::default(), 10, 0)?;
    assert_eq!(identities(&disabled.hits), identities(&incumbent.hits));
    assert!(disabled.titles.is_none());

    client.set_title_search_config(&fixture.data_dir(), title_config(1.0))?;
    let ranked = query(&client, TOPIC, SearchFilters::default(), 10, 0)?;
    assert_eq!(ranked.hits[0].conversation_id, Some(target.id));
    assert_eq!(ranked.hits.len(), 3);
    assert_eq!(
        ranked
            .hits
            .iter()
            .filter_map(|hit| hit.conversation_id)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([first.id, second.id, target.id])
    );
    for hit in &ranked.hits {
        assert_eq!(hit.match_type, MatchType::SemanticTitle);
        assert_eq!(hit.content, BODY);
        assert_eq!(hit.line_number, Some(1));
        assert_eq!(hit.created_at, Some(1_000));
        assert!(history.iter().any(|(_, conversation_id, body)| {
            Some(*conversation_id) == hit.conversation_id && body == &hit.content
        }));
    }
    let metadata = ranked.titles.context("request-owned title provenance")?;
    assert_eq!(metadata.indexed_titles, 3);
    assert_eq!(metadata.title_conversations, 3);
    assert_eq!(fixture.history()?, history);
    assert_eq!(fixture.title_bytes()?, titles_before);
    assert_eq!(
        fs::read(vector_index_path(&fixture.data_dir(), EMBEDDER))?,
        body_before
    );
    assert!(!fixture.data_dir().join("models").exists());
    Ok(())
}

#[test]
fn gh540_title_filters_apply_before_the_candidate_limit() -> Result<()> {
    let fixture = Fixture::new()?;
    let first = fixture.seed_in(
        "first",
        TOPIC,
        "codex",
        "project-one",
        "local",
        &[(MessageRole::User, 1_000, BODY)],
    )?;
    fixture
        .storage
        .upsert_source(&Source::remote("work-laptop", "work-host"))?;
    let target = fixture.seed_in(
        "target",
        TOPIC,
        "claude_code",
        "project-two",
        "work-laptop",
        &[(MessageRole::User, 2_000, BODY)],
    )?;
    fixture.build()?;
    let client = fixture.client(None)?;
    client.set_title_search_config(
        &fixture.data_dir(),
        TitleSearchConfig {
            title_candidates: 1,
            ..title_config(1.0)
        },
    )?;
    // Identical title vectors tie in canonical-ID order. Every selector below
    // must retrieve the second conversation even with a one-candidate budget.
    let cases = [
        SearchFilters {
            agents: HashSet::from(["claude_code".into()]),
            ..Default::default()
        },
        SearchFilters {
            workspaces: HashSet::from([target
                .conversation
                .workspace
                .as_ref()
                .context("workspace")?
                .to_string_lossy()
                .into_owned()]),
            ..Default::default()
        },
        SearchFilters {
            source_filter: SourceFilter::Remote,
            ..Default::default()
        },
        SearchFilters {
            source_filter: SourceFilter::SourceId("work-laptop".into()),
            ..Default::default()
        },
        SearchFilters {
            session_paths: HashSet::from([target
                .conversation
                .source_path
                .to_string_lossy()
                .into_owned()]),
            ..Default::default()
        },
        SearchFilters {
            created_from: Some(2_000),
            created_to: Some(2_000),
            ..Default::default()
        },
    ];
    for filters in cases {
        let description = format!("{filters:?}");
        let result = query(&client, TOPIC, filters, 1, 0)?;
        assert_eq!(result.hits.len(), 1, "{description}");
        assert_eq!(
            result.hits[0].conversation_id,
            Some(target.id),
            "{description}"
        );
        assert_eq!(result.hits[0].source_id, "work-laptop");
        assert_eq!(result.hits[0].origin_kind, "ssh");
    }
    let local = query(
        &client,
        TOPIC,
        SearchFilters {
            source_filter: SourceFilter::Local,
            ..Default::default()
        },
        1,
        0,
    )?;
    assert_eq!(local.hits[0].conversation_id, Some(first.id));
    let empty = query(
        &client,
        TOPIC,
        SearchFilters {
            agents: HashSet::from(["codex".into()]),
            source_filter: SourceFilter::Remote,
            ..Default::default()
        },
        1,
        0,
    )?;
    assert!(empty.hits.is_empty(), "filter predicates must intersect");
    Ok(())
}

#[test]
fn gh540_local_kind_sources_and_message_role_time_witnesses_are_respected() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut imported_local = Source::local();
    imported_local.id = "imported-backup".into();
    fixture.storage.upsert_source(&imported_local)?;
    let target = fixture.seed_in(
        "target",
        TOPIC,
        "shelley",
        "project-one",
        "imported-backup",
        &[
            (
                MessageRole::User,
                1_000,
                "The earlier prompt establishes the requested implementation.",
            ),
            (
                MessageRole::Agent,
                3_000,
                "The later answer records the actual verification and conclusion.",
            ),
        ],
    )?;
    fixture.build()?;
    let client = fixture.client(Some(HashSet::from([ROLE_ASSISTANT])))?;
    client.set_title_search_config(&fixture.data_dir(), title_config(1.0))?;
    let filters = SearchFilters {
        source_filter: SourceFilter::Local,
        created_from: Some(2_500),
        created_to: Some(3_500),
        ..Default::default()
    };
    let result = query(&client, TOPIC, filters.clone(), 10, 0)?;
    assert_eq!(result.hits.len(), 1);
    assert_eq!(result.hits[0].conversation_id, Some(target.id));
    assert_eq!(result.hits[0].source_id, "imported-backup");
    assert_eq!(result.hits[0].origin_kind, "local");
    assert_eq!(result.hits[0].line_number, Some(2));
    assert_eq!(result.hits[0].created_at, Some(3_000));
    assert_eq!(
        result.hits[0].content,
        target.conversation.messages[1].content
    );

    let tool_only = fixture.client(Some(HashSet::from([ROLE_TOOL])))?;
    tool_only.set_title_search_config(&fixture.data_dir(), title_config(1.0))?;
    assert!(query(&tool_only, TOPIC, filters, 10, 0)?.hits.is_empty());
    Ok(())
}

#[test]
fn gh540_title_mutation_excludes_stale_evidence_and_refreshes_retained_client() -> Result<()> {
    let fixture = Fixture::new()?;
    let changed = fixture.seed("changed", TOPIC)?;
    let survivor = fixture.seed("survivor", "Reliable backup schedules and retention")?;
    fixture.build()?;
    let client = fixture.client(None)?;
    client.set_title_search_config(&fixture.data_dir(), title_config(1.0))?;
    assert_eq!(
        query(&client, TOPIC, SearchFilters::default(), 10, 0)?.hits[0].conversation_id,
        Some(changed.id)
    );
    let body_before = fs::read(vector_index_path(&fixture.data_dir(), EMBEDDER))?;
    fixture.rename(
        &changed,
        "Revised topic about concurrent memory reclamation",
    )?;

    let stale = query(&client, TOPIC, SearchFilters::default(), 10, 0)?;
    assert_eq!(stale.hits.len(), 1);
    assert_eq!(stale.hits[0].conversation_id, Some(survivor.id));
    assert!(
        stale
            .titles
            .context("stale-title receipt")?
            .stale_titles_excluded
            >= 1
    );

    fixture.maintain_titles()?;
    let updated = query(
        &client,
        "Revised topic about concurrent memory reclamation",
        SearchFilters::default(),
        10,
        0,
    )?;
    assert_eq!(updated.hits[0].conversation_id, Some(changed.id));
    assert_eq!(
        updated.hits[0].title,
        "Revised topic about concurrent memory reclamation"
    );
    assert_eq!(
        fs::read(vector_index_path(&fixture.data_dir(), EMBEDDER))?,
        body_before
    );
    Ok(())
}

#[test]
fn gh540_pending_forget_fences_already_open_title_readers() -> Result<()> {
    let fixture = Fixture::new()?;
    let forgotten = fixture.seed("forgotten", TOPIC)?;
    fixture.seed("retained", "Rust ownership lifetimes and borrow checking")?;
    fixture.build()?;
    let client = fixture.client(None)?;
    client.set_title_search_config(&fixture.data_dir(), title_config(1.0))?;
    assert_eq!(
        query(&client, TOPIC, SearchFilters::default(), 10, 0)?.hits[0].conversation_id,
        Some(forgotten.id)
    );
    fixture.storage.forget_conversations_by_source_glob(
        forgotten
            .conversation
            .source_path
            .to_str()
            .context("fixture source path")?,
        false,
    )?;
    assert!(fixture.storage.forget_derived_rebuild_pending()?);
    let error = match query(&client, TOPIC, SearchFilters::default(), 10, 0) {
        Ok(_) => anyhow::bail!("an in-progress forget must refuse the retained title reader"),
        Err(error) => error,
    };
    assert!(
        format!("{error:#}").contains("forget-repair-pending"),
        "{error:#}"
    );
    Ok(())
}

#[test]
fn gh540_forget_fence_blocks_titles_before_canonical_deletion_commits() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.seed("pending-deletion", TOPIC)?;
    fixture.build()?;
    let history = fixture.history()?;
    let client = fixture.client(None)?;
    client.set_title_search_config(&fixture.data_dir(), title_config(1.0))?;
    assert_eq!(
        query(&client, TOPIC, SearchFilters::default(), 10, 0)?
            .hits
            .len(),
        1
    );

    // Forget publishes its durable fence before it changes canonical rows.
    // A retained title reader must honor this interval and failed deletions,
    // without relying on the later SQLite repair marker or body search.
    coding_agent_search::search::privacy::begin_forget(&fixture.db_path())?;
    let database_markers: Vec<String> = fixture.storage.raw().query_map_collect(
        "SELECT value FROM meta WHERE key = 'forget_derived_rebuild_pending'",
        &[],
        |row| row.get_typed(0),
    )?;
    assert!(database_markers.is_empty());
    assert_eq!(fixture.history()?, history);

    let error = match query(&client, TOPIC, SearchFilters::default(), 10, 0) {
        Ok(_) => anyhow::bail!("the pre-deletion privacy fence must refuse title-only queries"),
        Err(error) => error,
    };
    assert!(
        format!("{error:#}").contains("forget-repair-pending"),
        "{error:#}"
    );
    Ok(())
}

#[test]
fn gh540_reused_canonical_ids_cannot_resurrect_old_title_vectors() -> Result<()> {
    let fixture = Fixture::new()?;
    let original = fixture.seed("reused", TOPIC)?;
    fixture.build()?;
    let before = fixture.history()?;
    let client = fixture.client(None)?;
    client.set_title_search_config(&fixture.data_dir(), title_config(1.0))?;
    assert_eq!(
        query(&client, TOPIC, SearchFilters::default(), 10, 0)?
            .hits
            .len(),
        1
    );

    // Real deletion/re-insertion deliberately reuses every visible identity,
    // including title and body. Only the canonical invalidation witness can
    // distinguish the old title artifact from this new incarnation.
    let purged = fixture.storage.purge_agent_archive_data("shelley")?;
    assert_eq!(purged.conversations_deleted, 1);
    let replacement = fixture.seed("reused", TOPIC)?;
    assert_eq!(
        replacement.id, original.id,
        "fixture must exercise conversation ID reuse"
    );
    assert_eq!(
        fixture.history()?,
        before,
        "fixture must also reuse the message ID"
    );
    let stale = query(&client, TOPIC, SearchFilters::default(), 10, 0)?;
    assert!(stale.hits.is_empty());
    assert!(
        stale
            .titles
            .context("ID-reuse receipt")?
            .stale_titles_excluded
            >= 1
    );

    fixture.maintain_titles()?;
    let refreshed = query(&client, TOPIC, SearchFilters::default(), 10, 0)?;
    assert_eq!(refreshed.hits.len(), 1);
    assert_eq!(refreshed.hits[0].conversation_id, Some(replacement.id));
    Ok(())
}

#[test]
fn gh540_title_weights_pagination_and_projection_never_rewrite_artifacts() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.seed("first", "Rust ownership lifetimes and borrow checking")?;
    fixture.seed("second", "Network congestion control algorithms")?;
    let target = fixture.seed("target", TOPIC)?;
    fixture.build()?;
    let before = fixture.title_bytes()?;
    let client = fixture.client(None)?;
    for fusion in [TitleFusion::Weighted, TitleFusion::Max, TitleFusion::Rrf] {
        for weight in [0.25, 0.5, 1.0] {
            client.set_title_search_config(
                &fixture.data_dir(),
                TitleSearchConfig {
                    fusion,
                    ..title_config(weight)
                },
            )?;
            let full = query(&client, TOPIC, SearchFilters::default(), 10, 0)?;
            assert_eq!(
                full.hits[0].conversation_id,
                Some(target.id),
                "{fusion:?} weight={weight}"
            );
            let page = query(&client, TOPIC, SearchFilters::default(), 1, 1)?;
            assert_eq!(identities(&page.hits), identities(&full.hits[1..2]));
            let unlimited = query(&client, TOPIC, SearchFilters::default(), 0, 0)?;
            assert_eq!(identities(&unlimited.hits), identities(&full.hits));
            let minimal = client.search_semantic_with_metadata(
                TOPIC,
                SearchFilters::default(),
                10,
                0,
                FieldMask::new(false, false, false, false),
                false,
                SemanticTierMode::Single,
            )?;
            assert_eq!(identities(&minimal.hits), identities(&full.hits));
            assert!(minimal.hits.iter().all(|hit| hit.content.is_empty()
                && hit.snippet.is_empty()
                && hit.title.is_empty()));
            assert_eq!(fixture.title_bytes()?, before);
        }
    }
    assert_eq!(fixture.history()?.len(), 3);
    assert!(!fixture.data_dir().join("models").exists());
    Ok(())
}

#[test]
fn gh540_title_admission_budget_refuses_before_unbounded_retrieval() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.seed("one", TOPIC)?;
    fixture.seed("two", "Independent memory management investigation")?;
    fixture.build()?;
    let before = fixture.title_bytes()?;
    let client = fixture.client(None)?;
    client.set_title_search_config(
        &fixture.data_dir(),
        TitleSearchConfig {
            max_admission_titles: 1,
            ..title_config(1.0)
        },
    )?;
    let error = match query(&client, TOPIC, SearchFilters::default(), 10, 0) {
        Ok(_) => anyhow::bail!("two title records must not bypass a one-record admission budget"),
        Err(error) => error,
    };
    assert!(
        format!("{error:#}").contains("admission budget"),
        "{error:#}"
    );
    assert_eq!(fixture.title_bytes()?, before);
    assert!(title_manifest_path(&fixture.data_dir(), EMBEDDER)?.exists());
    Ok(())
}
