use super::*;
use crate::franken_sync::compat::ConnectionExt;
use crate::franken_sync::params;
use crate::indexer::semantic::{SemanticBackfillStoragePlan, SemanticIndexer};
use crate::indexer::semantic_progress::SemanticProgressSink;
use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use crate::search::model_manager::load_hash_semantic_context_strict;
use crate::search::semantic_manifest::{SemanticCurrentPointerV1, SemanticManifest, TierKind};
use crate::storage::sqlite::FrankenStorage;

struct Fixture {
    root: tempfile::TempDir,
    storage: FrankenStorage,
    agent: i64,
    conversations: Vec<(i64, Conversation)>,
}

fn message(index: i64, content: &str) -> Message {
    Message {
        id: None,
        idx: index,
        role: MessageRole::Agent,
        author: None,
        created_at: None,
        content: content.to_owned(),
        extra_json: serde_json::json!({}),
        snippets: Vec::new(),
    }
}

impl Fixture {
    fn new(contents: &[&str]) -> Result<Self> {
        let root = tempfile::tempdir()?;
        let storage = FrankenStorage::open(&root.path().join("agent_search.db"))?;
        let agent = storage.ensure_agent(&Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: None,
            kind: AgentKind::Cli,
        })?;
        let mut fixture = Self {
            root,
            storage,
            agent,
            conversations: Vec::new(),
        };
        for (number, content) in contents.iter().enumerate() {
            fixture.insert(&format!("conversation-{number}"), content)?;
        }
        fixture.publish()?;
        Ok(fixture)
    }

    fn insert(&mut self, name: &str, content: &str) -> Result<i64> {
        let conversation = Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: None,
            external_id: Some(name.to_owned()),
            title: Some(name.to_owned()),
            source_path: self.root.path().join(format!("{name}.jsonl")),
            started_at: None,
            ended_at: None,
            approx_tokens: None,
            metadata_json: serde_json::json!({}),
            messages: vec![message(0, content)],
            source_id: "local".into(),
            origin_host: None,
        };
        let outcome = self
            .storage
            .insert_conversation_tree(self.agent, None, &conversation)?;
        self.conversations
            .push((outcome.conversation_id, conversation));
        Ok(outcome.conversation_id)
    }

    fn publish(&self) -> Result<()> {
        let indexer = SemanticIndexer::new("hash", None)?;
        let mut manifest = SemanticManifest::load_or_default(self.root.path())?;
        let outcome = indexer.run_backfill_from_storage_with_sink(
            &self.storage,
            self.root.path(),
            &mut manifest,
            SemanticBackfillStoragePlan {
                tier: TierKind::Fast,
                db_fingerprint: crate::indexer::lexical_storage_fingerprint_for_storage(
                    &self.storage,
                )?,
                model_revision: "hash".into(),
                max_conversations: usize::MAX,
            },
            &SemanticProgressSink::disabled(),
        )?;
        assert!(outcome.published);
        assert!(manifest.coverage.contains_key(indexer.embedder_id()));
        Ok(())
    }

    fn attach_context(&self, client: &SearchClient) -> Result<()> {
        let path = self.root.path().join("agent_search.db");
        let setup = load_hash_semantic_context_strict(self.root.path(), &path);
        let context = setup
            .context
            .ok_or_else(|| anyhow!("{}", setup.availability.summary()))?;
        client.set_semantic_artifacts_context(
            context.embedder,
            context.artifacts,
            context.quality_artifact,
            context.filter_maps,
            context.roles,
        )
    }

    fn unattached_client(&self) -> Result<SearchClient> {
        SearchClient::open_semantic(
            &self.root.path().join("agent_search.db"),
            SearchClientOptions {
                enable_reload: false,
                enable_warm: false,
                strict_read_only: true,
            },
        )?
        .context("fixture archive is missing")
    }

    fn client(&self) -> Result<SearchClient> {
        let client = self.unattached_client()?;
        self.attach_context(&client)?;
        Ok(client)
    }

    fn copied_database(&mut self) -> Result<PathBuf> {
        self.storage
            .raw()
            .query("PRAGMA wal_checkpoint(TRUNCATE)")?;
        self.storage.close_without_checkpoint_in_place()?;
        let copied = self.root.path().join("copied-archive.db");
        std::fs::copy(self.root.path().join("agent_search.db"), &copied)?;
        Ok(copied)
    }
}

fn search(
    client: &SearchClient,
    limit: usize,
) -> Result<(Vec<SearchHit>, SemanticCoverageSnapshot)> {
    let (hits, stats, coverage) = client.search_semantic_with_tier_and_coverage(
        "retained semantic coverage",
        SearchFilters::default(),
        limit,
        0,
        FieldMask::new(true, true, true, true),
        false,
        SemanticTierMode::Single,
    )?;
    assert!(stats.is_none());
    Ok((
        hits,
        coverage.context("certified query omitted its coverage receipt")?,
    ))
}

#[test]
fn certified_semantic_retains_complete_generation_across_ingest_and_publication() -> Result<()> {
    let mut fixture = Fixture::new(&["retained semantic coverage first passage"])?;
    let old_client = fixture.client()?;
    let (initial, complete) = search(&old_client, 10)?;
    assert_eq!(initial.len(), 1);
    assert_eq!(complete.status, "complete");
    fixture.insert(
        "new-conversation",
        "new retained semantic coverage second passage",
    )?;
    let (old_hits, partial) = search(&old_client, 10)?;
    assert_eq!(old_hits.len(), 1);
    assert_eq!(partial.status, "partial");
    assert_eq!(partial.generation, complete.generation);
    assert!(partial.current_mutation_seq > complete.current_mutation_seq);
    fixture.publish()?;
    let (current, newer) = search(&fixture.client()?, 10)?;
    assert_eq!(current.len(), 2);
    assert_eq!(newer.status, "complete");
    assert_ne!(newer.generation, partial.generation);
    // A concurrent publisher cannot replace or relabel an already retained owner.
    let (retained, receipt) = search(&old_client, 10)?;
    assert_eq!(retained.len(), 1);
    assert_eq!(receipt.generation, partial.generation);
    assert_eq!(complete.status, "complete"); // Request-local receipt was not mutated.
    Ok(())
}

#[test]
fn certified_semantic_warm_client_allows_append_but_refuses_database_path_changes() -> Result<()> {
    let mut fixture = Fixture::new(&["retained semantic coverage owned archive"])?;
    let client = fixture.unattached_client()?;
    // Open the actual cached hydration connection through a lexical operation,
    // before any semantic context exists on this client.
    let lexical = client.browse_by_date(SearchFilters::default(), 10, 0, true, FieldMask::FULL)?;
    assert_eq!(lexical.len(), 1);
    fixture.attach_context(&client)?;
    let (_, complete) = search(&client, 10)?;
    assert_eq!(complete.status, "complete");
    fixture.insert("ordinary-append", "new unembedded canonical passage")?;
    let (hits, partial) = search(&client, 10)?;
    assert_eq!(hits.len(), 1);
    assert_eq!(partial.status, "partial");
    assert_eq!(partial.generation, complete.generation);
    assert!(partial.current_mutation_seq > complete.current_mutation_seq);

    // The replacement has the same bytes and archive_id, so the SQL authority
    // check alone cannot reject continued use of the old open connection.
    let replacement = fixture.copied_database()?;
    let path = fixture.root.path().join("agent_search.db");
    let retained = fixture.root.path().join("retained-archive.db");
    std::fs::rename(&path, &retained)?;
    std::fs::rename(&replacement, &path)?;
    let changed = search(&client, 10).unwrap_err();
    assert!(
        changed
            .to_string()
            .contains("canonical archive file changed")
    );
    assert!(
        !same_file::is_same_file(&path, &retained)?,
        "replacement must have a different file identity"
    );
    // The failed request has evicted its owner. A separate request can open
    // this byte-identical incarnation and validate its certificate normally.
    assert_eq!(search(&client, 10)?.1.status, "partial");
    let replacement_retained = fixture.root.path().join("replacement-retained.db");
    std::fs::rename(&path, &replacement_retained)?;
    let missing = search(&client, 10).unwrap_err();
    assert!(missing.to_string().contains("canonical archive"));

    std::os::unix::fs::symlink(&replacement_retained, &path)?;
    let alias = search(&client, 10).unwrap_err();
    assert!(alias.to_string().contains("canonical archive ownership"));
    Ok(())
}

#[test]
fn certified_semantic_attachment_cannot_rebind_an_existing_hydration_owner() -> Result<()> {
    let mut fixture = Fixture::new(&["retained semantic coverage prior lexical owner"])?;
    let client = fixture.unattached_client()?;
    let path = fixture.root.path().join("agent_search.db");
    let context = load_hash_semantic_context_strict(fixture.root.path(), &path)
        .context
        .context("fixture semantic context")?;
    assert_eq!(
        client
            .browse_by_date(SearchFilters::default(), 10, 0, true, FieldMask::FULL)?
            .len(),
        1
    );
    let replacement = fixture.copied_database()?;
    std::fs::rename(&path, fixture.root.path().join("prior-archive.db"))?;
    std::fs::rename(&replacement, &path)?;
    // Attaching an already admitted context after replacement must not transfer
    // the previous connection's witness to the new, byte-identical pathname.
    client.set_semantic_artifacts_context(
        context.embedder,
        context.artifacts,
        context.quality_artifact,
        context.filter_maps,
        context.roles,
    )?;
    let error = search(&client, 10).unwrap_err();
    assert!(error.to_string().contains("canonical archive file changed"));
    Ok(())
}

#[test]
fn certified_semantic_opening_seal_detects_ancestor_rename_restore_through_parent_symlink()
-> Result<()> {
    let root = tempfile::tempdir()?;
    let target = root.path().join("target");
    let alias = root.path().join("alias");
    std::fs::create_dir(&target)?;
    std::os::unix::fs::symlink(&target, &alias)?;
    let path = alias.join("archive.db");
    std::fs::write(&path, b"database opening seal fixture")?;

    // A stable symlink in a parent path is supported. Only a final-component
    // database symlink is refused.
    CanonicalDatabaseOpening::capture(&path)?
        .finish()?
        .verify()?;
    let opening = CanonicalDatabaseOpening::capture(&path)?;
    let file_before = std::fs::metadata(&path)?;
    let target_before = std::fs::metadata(&target)?;
    let moved = root.path().join("moved-target");
    std::fs::rename(&target, &moved)?;
    // Make the directory's change deterministic even on filesystems whose
    // timestamp resolution can merge the two renames into one clock tick.
    std::fs::File::open(&moved)?.set_times(
        std::fs::FileTimes::new().set_modified(target_before.modified()? + Duration::from_secs(2)),
    )?;
    std::fs::rename(&moved, &target)?;
    crate::search::semantic_coverage::ensure_same_file_metadata(
        &file_before,
        &std::fs::metadata(&path)?,
    )?;
    let error = opening.finish().unwrap_err();
    assert!(error.to_string().contains("ancestor changed"));
    Ok(())
}

#[test]
fn certified_semantic_retries_with_a_new_owner_after_an_unavailable_opening_seal() -> Result<()> {
    let mut fixture = Fixture::new(&["retained semantic coverage opening recovery"])?;
    let client = fixture.client()?;
    let path = fixture.root.path().join("agent_search.db");
    {
        let mut cached = client.sqlite_guard()?;
        let connection = cached.as_mut().context("fixture hydration owner")?;
        let opening = CanonicalDatabaseOpening::capture(&path)?;
        let parent = path.parent().context("fixture archive parent")?;
        let before = std::fs::metadata(parent)?;
        std::fs::File::open(parent)?.set_times(
            std::fs::FileTimes::new().set_modified(before.modified()? + Duration::from_secs(2)),
        )?;
        // Install the actual failed-seal result that an opening race leaves in
        // the cached owner; the database and its dedicated worker are real.
        connection.archive_witness = opening.finish().ok();
        assert!(connection.archive_witness.is_none());
    }
    fixture.insert("recovery-append", "new unembedded recovery passage")?;
    let rejected = search(&client, 10).unwrap_err();
    assert!(rejected.to_string().contains("retry the search"));
    assert!(
        client
            .sqlite
            .lock()
            .map_err(|_| anyhow!("fixture sqlite lock poisoned"))?
            .is_none(),
        "a failed opening seal must evict its actual cached owner"
    );
    let (hits, partial) = search(&client, 10)?;
    assert_eq!(hits.len(), 1);
    assert_eq!(partial.status, "partial");
    assert!(partial.current_mutation_seq > partial.published_mutation_seq);
    assert_eq!(search(&client, 10)?.1.generation, partial.generation);
    Ok(())
}

#[test]
fn certified_semantic_concurrent_ingestion_keeps_request_payloads_and_receipts_coherent()
-> Result<()> {
    let fixture = Fixture::new(&["retained semantic coverage original concurrent passage"])?;
    let client = fixture.client()?;
    let original = fixture.conversations[0].0;
    let archive = fixture.root.path().join("agent_search.db");
    let mut conversation = fixture.conversations[0].1.clone();
    let agent = fixture.agent;
    let start = Arc::new(std::sync::Barrier::new(2));
    let writer_start = Arc::clone(&start);
    let writer = std::thread::spawn(move || -> Result<()> {
        writer_start.wait();
        let storage = FrankenStorage::open(&archive)?;
        for index in 1..=4 {
            conversation
                .messages
                .push(message(index, "new unembedded concurrent passage"));
            storage.insert_conversation_tree(agent, None, &conversation)?;
        }
        Ok(())
    });
    start.wait();
    let mut receipts = Vec::new();
    for _ in 0..4 {
        let (hits, receipt) = search(&client, 10)?;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].conversation_id, Some(original));
        assert!(hits[0].content.contains("original concurrent"));
        assert!(receipt.current_mutation_seq >= receipt.published_mutation_seq);
        receipts.push(receipt);
    }
    writer
        .join()
        .map_err(|_| anyhow!("ingestion thread panicked"))??;
    let (hits, partial) = search(&client, 10)?;
    assert_eq!(hits.len(), 1);
    assert_eq!(partial.status, "partial");
    assert!(
        receipts
            .iter()
            .all(|receipt| receipt.generation == partial.generation)
    );
    Ok(())
}

#[test]
fn certified_semantic_raw_parent_changes_followed_by_ingest_cannot_rebind_old_scores() -> Result<()>
{
    for statement in [
        "UPDATE conversations SET source_path = 'reidentified.jsonl' WHERE id = ?1",
        "UPDATE conversations SET external_id = 'reidentified' WHERE id = ?1",
        "UPDATE conversations SET title = 'new canonical title' WHERE id = ?1",
        "UPDATE conversations SET started_at = 0 WHERE id = ?1",
        "UPDATE messages SET created_at = 0 WHERE conversation_id = ?1",
        "UPDATE messages SET conversation_id = 2, idx = 4 WHERE conversation_id = ?1",
    ] {
        let mut fixture = Fixture::new(&[
            "retained semantic coverage affected passage",
            "retained semantic coverage unaffected passage",
        ])?;
        let affected = fixture.conversations[0].0;
        let client = fixture.client()?;
        fixture
            .storage
            .raw()
            .execute_compat(statement, params![affected])?;
        fixture.insert("tracked-append", "later tracked append semantic coverage")?;
        let (hits, coverage) = search(&client, 10)?;
        assert_eq!(hits.len(), 1, "{statement}");
        assert_eq!(hits[0].conversation_id, Some(fixture.conversations[1].0));
        assert!(hits[0].content.contains("unaffected"));
        assert_eq!(coverage.status, "partial");
        assert_eq!(coverage.excluded_candidates, 1);
    }
    Ok(())
}

#[test]
fn certified_semantic_changed_content_and_forgotten_messages_are_excluded_and_refilled()
-> Result<()> {
    let fixture = Fixture::new(&[
        "retained semantic coverage matching first passage",
        "retained semantic coverage matching second passage",
        "retained semantic coverage surviving third passage",
    ])?;
    let client = fixture.client()?;
    fixture.storage.raw().execute_compat(
        "UPDATE messages SET content = 'replaced unrelated canonical body' WHERE conversation_id = ?1",
        params![fixture.conversations[0].0],
    )?;
    fixture.storage.forget_conversations_by_source_glob(
        fixture.conversations[1]
            .1
            .source_path
            .to_str()
            .context("fixture path")?,
        false,
    )?;
    let (hits, coverage) = search(&client, 1)?;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].conversation_id, Some(fixture.conversations[2].0));
    assert!(hits[0].content.contains("surviving"));
    assert_eq!(coverage.status, "partial");
    assert!(!coverage.candidate_window_limited);
    Ok(())
}

#[test]
fn certified_semantic_row_id_reuse_never_transfers_a_vector_to_another_conversation() -> Result<()>
{
    let mut fixture = Fixture::new(&[
        "retained semantic coverage stable first message",
        "retained semantic coverage identical reused message",
    ])?;
    let client = fixture.client()?;
    let deleted = fixture.storage.fetch_messages(fixture.conversations[1].0)?[0].id;
    fixture.storage.forget_conversations_by_source_glob(
        fixture.conversations[1]
            .1
            .source_path
            .to_str()
            .context("fixture path")?,
        false,
    )?;
    let replacement = fixture.insert(
        "replacement-parent",
        "retained semantic coverage identical reused message",
    )?;
    let replacement_id = fixture.storage.fetch_messages(replacement)?[0].id;
    assert_eq!(
        replacement_id, deleted,
        "fixture must exercise numeric ID reuse"
    );
    let (hits, coverage) = search(&client, 10)?;
    assert!(
        hits.iter()
            .all(|hit| hit.conversation_id != Some(replacement))
    );
    assert_eq!(hits.len(), 1);
    assert_eq!(coverage.excluded_candidates, 1);
    Ok(())
}

#[test]
fn certified_semantic_current_pointer_transition_is_refused_by_warm_and_cold_loaders() -> Result<()>
{
    let fixture = Fixture::new(&["retained semantic coverage pointer transition"])?;
    let client = fixture.client()?;
    assert_eq!(search(&client, 1)?.0.len(), 1);
    let pointer = SemanticCurrentPointerV1::path(fixture.root.path());
    std::fs::create_dir_all(pointer.parent().context("pointer parent")?)?;
    std::fs::write(pointer, b"{}")?;
    assert!(
        search(&client, 1)
            .unwrap_err()
            .to_string()
            .contains("owner_backed_reader_required")
    );
    assert!(fixture.client().is_err());
    Ok(())
}

#[test]
fn certified_semantic_passage_winner_retains_its_full_identity() {
    let identity = SemanticDocId {
        message_id: 4,
        chunk_idx: 3,
        agent_id: 2,
        workspace_id: 1,
        source_id: 3,
        role: 1,
        created_at_ms: 9,
        content_hash: Some([7; 32]),
    };
    let mut weaker = identity;
    weaker.chunk_idx = 0;
    weaker.content_hash = Some([1; 32]);
    let mut winners = HashMap::new();
    for (parsed, score) in [(weaker, 0.2), (identity, 0.9)] {
        SearchClient::record_fs_semantic_hit(
            &mut winners,
            &FsVectorHit {
                index: 0,
                score,
                doc_id: parsed.to_doc_id_string().into(),
            },
        );
    }
    assert_eq!(winners[&4].identity, Some(identity));
    assert_eq!(winners[&4].chunk_idx, 3);
}

#[test]
fn certified_semantic_raw_role_change_uses_the_producers_canonical_normalization() -> Result<()> {
    let fixture = Fixture::new(&["retained semantic coverage role identity"])?;
    fixture
        .storage
        .raw()
        .execute("UPDATE messages SET role = 'user'")?;
    fixture.publish()?;
    let client = fixture.client()?;
    assert_eq!(search(&client, 10)?.0.len(), 1);
    fixture
        .storage
        .raw()
        .execute("UPDATE messages SET role = ' Assistant '")?;
    let (hits, receipt) = search(&client, 10)?;
    assert!(hits.is_empty());
    assert_eq!(receipt.excluded_candidates, 1);
    assert_eq!(receipt.status, "partial");
    Ok(())
}

fn receipt() -> SemanticCoverageSnapshot {
    SemanticCoverageSnapshot {
        status: "partial",
        generation: "fixture-generation".into(),
        published_at_ms: 1,
        published_mutation_seq: 1,
        current_mutation_seq: 2,
        published_max_message_id: 1,
        current_max_message_id: 2,
        indexed_passages: 1,
        excluded_candidates: 0,
        candidate_window_limited: false,
        stale_reason: Some("archive_changed_since_publication"),
    }
}

fn hit() -> SearchHit {
    SearchHit {
        title: "retained title".into(),
        snippet: "retained snippet".into(),
        content: "retained semantic coverage payload".into(),
        content_hash: 42,
        score: 1.0,
        source_path: "/retained/session.jsonl".into(),
        agent: "codex".into(),
        workspace: "/retained/workspace".into(),
        workspace_original: None,
        created_at: Some(1),
        line_number: Some(1),
        match_type: MatchType::Exact,
        source_id: "local".into(),
        origin_kind: "local".into(),
        origin_host: None,
        conversation_id: Some(1),
    }
}

fn assert_payload(actual: &SearchHit, expected: &SearchHit) {
    assert_eq!(actual.content_hash, expected.content_hash);
    let mut actual = serde_json::to_value(actual).unwrap();
    let mut expected = serde_json::to_value(expected).unwrap();
    actual.as_object_mut().unwrap().remove("score");
    expected.as_object_mut().unwrap().remove("score");
    assert_eq!(actual, expected);
    assert_eq!(actual["content"], expected["content"]);
}

#[test]
fn certified_rrf_exact_key_preserves_semantic_payload_despite_lexical_metadata_drift() {
    let semantic = hit();
    let mut lexical = semantic.clone();
    lexical.title = "later title".into();
    lexical.workspace = "/later/workspace".into();
    lexical.agent = "later-agent".into();
    lexical.snippet = "later snippet".into();
    assert_eq!(search_hit_doc_id(&lexical), search_hit_doc_id(&semantic));
    let fused = rrf_fuse_hits_with_coverage(
        &[lexical.clone()],
        &[semantic.clone()],
        "retained",
        10,
        0,
        Some(&receipt()),
    );
    assert_eq!(fused.len(), 1);
    assert_payload(&fused[0], &semantic);
    assert_payload(
        &rrf_fuse_hits(&[lexical.clone()], &[semantic], "retained", 10, 0)[0],
        &lexical,
    );
}

#[test]
fn certified_rrf_compatibility_dedup_never_fills_semantic_identity_from_lexical() {
    for semantic_has_id in [true, false] {
        let mut semantic = hit();
        let mut lexical = semantic.clone();
        if semantic_has_id {
            lexical.conversation_id = None;
        } else {
            semantic.conversation_id = None;
        }
        lexical.workspace = "/different/workspace".into();
        assert_ne!(search_hit_doc_id(&lexical), search_hit_doc_id(&semantic));
        let fused = rrf_fuse_hits_with_coverage(
            &[lexical],
            &[semantic.clone()],
            "retained",
            10,
            0,
            Some(&receipt()),
        );
        assert_eq!(fused.len(), 1);
        assert_payload(&fused[0], &semantic);
    }
}

#[test]
fn certified_rrf_keeps_distinct_canonical_identities_and_content_separate() {
    let semantic = hit();
    for change_identity in [true, false] {
        let mut lexical = semantic.clone();
        if change_identity {
            lexical.conversation_id = Some(2);
        } else {
            lexical.content_hash += 1;
            lexical.content = "different canonical body".into();
        }
        let fused = rrf_fuse_hits_with_coverage(
            &[lexical],
            &[semantic.clone()],
            "retained",
            10,
            0,
            Some(&receipt()),
        );
        assert_eq!(fused.len(), 2);
        assert!(
            fused
                .iter()
                .any(|hit| hit.content_hash == semantic.content_hash
                    && hit.conversation_id == semantic.conversation_id)
        );
    }
}

#[test]
fn certified_rrf_preserves_shaped_empty_fields_and_deterministic_semantic_duplicate() {
    let lexical = hit();
    let mut semantic = lexical.clone();
    semantic.title.clear();
    semantic.content.clear();
    semantic.snippet = "certified preview".into();
    let mut repeated = semantic.clone();
    repeated.snippet = "last certified preview".into();
    let fused = rrf_fuse_hits_with_coverage(
        &[lexical],
        &[semantic, repeated.clone()],
        "retained",
        10,
        0,
        Some(&receipt()),
    );
    assert_eq!(fused.len(), 1);
    assert_payload(&fused[0], &repeated);
}
