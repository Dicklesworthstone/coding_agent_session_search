//! Real canonical transactions for the GH #458 coverage journal and proofs.

use super::*;

struct Fixture {
    _directory: tempfile::TempDir,
    storage: FrankenStorage,
    agent: i64,
    conversation: Conversation,
    conversation_id: i64,
}

impl Fixture {
    fn new() -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let storage = FrankenStorage::open(&directory.path().join("archive.db"))?;
        let agent = storage.ensure_agent(&Agent {
            id: None,
            slug: "codex".to_owned(),
            name: "Codex".to_owned(),
            version: None,
            kind: AgentKind::Cli,
        })?;
        let conversation = Conversation {
            id: None,
            agent_slug: "codex".to_owned(),
            workspace: None,
            external_id: Some("semantic-coverage".to_owned()),
            title: Some("Semantic coverage".to_owned()),
            source_path: PathBuf::from("/fixture/semantic-coverage.jsonl"),
            started_at: Some(1000),
            ended_at: None,
            approx_tokens: None,
            metadata_json: serde_json::Value::Null,
            messages: vec![Self::message(0)],
            source_id: LOCAL_SOURCE_ID.to_owned(),
            origin_host: None,
        };
        let outcome =
            storage.insert_conversation_tree_with_analytics(agent, None, &conversation, true)?;
        Ok(Self {
            _directory: directory,
            storage,
            agent,
            conversation,
            conversation_id: outcome.conversation_id,
        })
    }

    fn message(idx: i64) -> Message {
        Message {
            id: None,
            idx,
            role: MessageRole::User,
            author: None,
            created_at: None,
            content: format!("Canonical coverage message {idx}"),
            extra_json: serde_json::Value::Null,
            snippets: Vec::new(),
        }
    }

    fn append(&mut self) -> Result<()> {
        let next = i64::try_from(self.conversation.messages.len())?;
        self.conversation.messages.push(Self::message(next));
        self.storage.insert_conversation_tree_with_analytics(
            self.agent,
            None,
            &self.conversation,
            true,
        )?;
        Ok(())
    }

    fn state(&self) -> Result<SemanticArchiveState> {
        self.storage
            .semantic_archive_state()?
            .context("archive authority")
    }

    fn proof(&self) -> Result<[u8; 32]> {
        lookup_semantic_parent_identities_in_connection(self.storage.raw(), &[1])?
            .remove(&1)
            .context("first canonical message proof")
    }
}

#[test]
fn gh458_append_advances_delta_without_invalidating_retained_messages() -> Result<()> {
    let mut fixture = Fixture::new()?;
    let before = fixture.state()?;
    let proof = fixture.proof()?;
    fixture.append()?;
    let after = fixture.state()?;
    assert_eq!(before.archive_id, after.archive_id);
    assert!(after.mutation_seq > before.mutation_seq);
    assert!(!fixture.storage.semantic_invalidations_after(&before)?);
    let changes = fixture
        .storage
        .semantic_conversation_changes_since(&before)?
        .context("delta")?;
    assert!(changes.complete);
    assert_eq!(changes.state, after);
    assert_eq!(changes.conversation_ids, [fixture.conversation_id]);
    assert_eq!(fixture.proof()?, proof);
    fixture.storage.insert_conversation_tree_with_analytics(
        fixture.agent,
        None,
        &fixture.conversation,
        true,
    )?;
    assert_eq!(
        fixture.state()?,
        after,
        "duplicate ingest must not advance semantic work"
    );
    Ok(())
}

#[test]
fn gh458_failed_ingest_rolls_back_rows_journal_and_message_high_water() -> Result<()> {
    let fixture = Fixture::new()?;
    let before = fixture.state()?;
    let high_water = fixture
        .storage
        .raw()
        .query_row("SELECT value FROM meta WHERE key = 'semantic_message_id_high_water_v1'")?
        .get_typed::<String>(0)?;
    {
        let tx = fixture.storage.raw().transaction()?;
        let next = Fixture::message(1);
        franken_batch_insert_new_messages(&tx, fixture.conversation_id, &[&next])?;
        assert_ne!(
            semantic_archive_state_with_query(|sql| Ok(tx.query(sql)?))?,
            Some(before.clone())
        );
        assert!(franken_batch_insert_new_messages(&tx, fixture.conversation_id, &[&next]).is_err());
        // RAII rollback includes the first insert and its already-written journal.
    }
    assert_eq!(fixture.state()?, before);
    assert_eq!(fixture.storage.total_message_count()?, 1);
    assert_eq!(
        fixture
            .storage
            .raw()
            .query_row("SELECT value FROM meta WHERE key = 'semantic_message_id_high_water_v1'")?
            .get_typed::<String>(0)?,
        high_water
    );
    assert!(
        fixture
            .storage
            .semantic_conversation_changes_since(&before)?
            .context("delta")?
            .conversation_ids
            .is_empty()
    );
    Ok(())
}

#[test]
fn gh458_forget_and_reused_ids_keep_durable_invalidation() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.append()?;
    let before = fixture.state()?;
    fixture.storage.set_last_embedded_message_id("fast", 2)?;
    fixture.storage.set_last_embedded_message_id("quality", 2)?;
    let forgotten = fixture
        .storage
        .forget_conversations_by_source_glob("/fixture/semantic-coverage.jsonl", false)?;
    assert_eq!(forgotten.conversations_deleted, 1);
    let after_delete = fixture.state()?;
    assert!(fixture.storage.semantic_invalidations_after(&before)?);
    assert_eq!(fixture.storage.get_last_embedded_message_id("fast")?, None);
    assert_eq!(
        fixture.storage.get_last_embedded_message_id("quality")?,
        None
    );
    assert!(
        !fixture
            .storage
            .set_last_embedded_message_id_if_unchanged("fast", 2, &before)?
    );
    let mut replacement = fixture.conversation.clone();
    replacement.external_id = Some("replacement".to_owned());
    replacement.source_path = PathBuf::from("/fixture/replacement.jsonl");
    replacement.messages.truncate(1);
    let outcome = fixture.storage.insert_conversation_tree_with_analytics(
        fixture.agent,
        None,
        &replacement,
        true,
    )?;
    assert_eq!(
        fixture.storage.max_message_id()?,
        Some(1),
        "exercise SQLite top-rowid reuse"
    );
    assert!(
        fixture
            .storage
            .semantic_invalidations_after(&after_delete)?
    );
    let invalidated: i64 = fixture.storage.raw().query_row_map(
        "SELECT invalidation_seq FROM semantic_conversation_changes WHERE conversation_id = ?1",
        fparams![outcome.conversation_id],
        |row| row.get_typed(0),
    )?;
    assert!(invalidated as u64 > after_delete.mutation_seq);
    assert_eq!(
        fixture
            .storage
            .raw()
            .query_row("SELECT value FROM meta WHERE key = 'semantic_message_id_high_water_v1'")?
            .get_typed::<String>(0)?,
        "2"
    );
    Ok(())
}

#[test]
fn gh458_orphan_repair_invalidates_coverage_in_its_delete_transaction() -> Result<()> {
    let fixture = Fixture::new()?;
    let before = fixture.state()?;
    fixture.storage.set_last_embedded_message_id("fast", 1)?;
    fixture.storage.raw().execute("PRAGMA foreign_keys = OFF")?;
    fixture
        .storage
        .raw()
        .execute("UPDATE messages SET conversation_id = 99999 WHERE id = 1")?;
    fixture.storage.raw().execute("PRAGMA foreign_keys = ON")?;
    assert!(delete_orphan_message_id_chunk_once(fixture.storage.raw(), &[1])? > 0);
    assert_eq!(fixture.storage.get_last_embedded_message_id("fast")?, None);
    assert!(fixture.storage.semantic_invalidations_after(&before)?);
    assert!(
        !fixture
            .storage
            .semantic_conversation_changes_since(&before)?
            .context("delta")?
            .complete
    );
    assert!(
        !fixture
            .storage
            .set_last_embedded_message_id_if_unchanged("fast", 1, &before)?
    );
    Ok(())
}

#[test]
fn gh458_completion_and_each_embedder_watermark_use_transactional_cas() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture
        .storage
        .mark_semantic_identity_rebuild_required(SemanticIdentityTier::Fast)?;
    let old = fixture.state()?;
    fixture.append()?;
    assert!(
        !fixture
            .storage
            .complete_semantic_identity_rebuild_if_current(SemanticIdentityTier::Fast, &old)?
    );
    assert!(
        fixture
            .storage
            .semantic_identity_rebuild_required(SemanticIdentityTier::Fast)?
    );
    assert!(
        !fixture
            .storage
            .set_last_embedded_message_id_if_unchanged("fast", 1, &old)?
    );
    let current = fixture.state()?;
    assert!(
        fixture
            .storage
            .complete_semantic_identity_rebuild_if_current(SemanticIdentityTier::Fast, &current)?
    );
    assert!(
        !fixture
            .storage
            .complete_semantic_identity_rebuild_if_current(SemanticIdentityTier::Fast, &current)?
    );
    assert!(
        fixture
            .storage
            .set_last_embedded_message_id_if_unchanged("fast", 2, &current)?
    );
    assert!(
        !fixture
            .storage
            .set_last_embedded_message_id_if_unchanged("fast", 2, &current)?
    );
    assert_eq!(
        fixture.storage.get_last_embedded_message_id("quality")?,
        None
    );
    assert!(
        fixture
            .storage
            .set_last_embedded_message_id_if_unchanged("quality", 1, &current)?
    );
    assert_eq!(
        fixture.state()?,
        current,
        "derived bookkeeping is not a canonical mutation"
    );
    Ok(())
}

#[test]
fn gh458_parent_proofs_bind_nullable_timestamps_and_raw_parent_identity() -> Result<()> {
    let fixture = Fixture::new()?;
    let initial = fixture.proof()?;
    fixture
        .storage
        .raw()
        .execute("UPDATE messages SET created_at = 0 WHERE id = 1")?;
    let epoch = fixture.proof()?;
    assert_ne!(initial, epoch, "None must not collapse into epoch zero");
    fixture
        .storage
        .raw()
        .execute("UPDATE messages SET created_at = NULL WHERE id = 1")?;
    assert_eq!(initial, fixture.proof()?);
    fixture
        .storage
        .raw()
        .execute("UPDATE conversations SET started_at = 2000 WHERE id = 1")?;
    assert_ne!(initial, fixture.proof()?);
    let timestamp = fixture.proof()?;
    fixture.storage.raw().execute("UPDATE conversations SET title = 'reidentified title', external_id = 'different-native-session', source_path = '/different/source' WHERE id = 1")?;
    assert_ne!(timestamp, fixture.proof()?);
    Ok(())
}

#[test]
fn gh458_parent_proofs_bind_raw_role_even_when_normalized_role_matches() -> Result<()> {
    let fixture = Fixture::new()?;
    let initial = fixture.proof()?;
    let state = fixture.storage.semantic_archive_state()?;
    fixture
        .storage
        .raw()
        .execute("UPDATE messages SET role = ' User ' WHERE id = 1")?;
    assert_ne!(
        initial,
        fixture.proof()?,
        "canonical role aliases must not hide an untracked raw identity edit"
    );
    assert_eq!(state, fixture.storage.semantic_archive_state()?);
    fixture
        .storage
        .raw()
        .execute("UPDATE messages SET role = 'user' WHERE id = 1")?;
    assert_eq!(initial, fixture.proof()?);
    Ok(())
}

#[test]
fn gh458_parent_native_identity_uses_canonical_binary_precedence() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture
        .storage
        .raw()
        .execute("UPDATE agents SET slug = 'codebuff' WHERE id = 1")?;
    let binary = serialize_json_to_msgpack(
        &serde_json::json!({"id":"native-a", "codebuff_message_id":"native-a"}),
    )
    .context("native envelope")?;
    fixture.storage.raw().execute_compat(
        "UPDATE messages SET extra_json = ?1, extra_bin = ?2 WHERE id = 1",
        fparams![
            r#"{"id":"native-b","codebuff_message_id":"native-b"}"#,
            binary.as_slice()
        ],
    )?;
    let binary_proof = fixture.proof()?;
    fixture
        .storage
        .raw()
        .execute("UPDATE messages SET extra_json = '{}' WHERE id = 1")?;
    assert_eq!(fixture.proof()?, binary_proof);
    fixture
        .storage
        .raw()
        .execute("UPDATE messages SET extra_bin = NULL WHERE id = 1")?;
    assert!(
        fixture.proof().is_err(),
        "missing native identity cannot acquire a parent proof"
    );
    Ok(())
}

#[test]
fn gh458_parent_stream_and_candidate_lookup_share_bounded_projection() -> Result<()> {
    let fixture = Fixture::new()?;
    let messages = (1..1026).map(Fixture::message).collect::<Vec<_>>();
    let references = messages.iter().collect::<Vec<_>>();
    let mut write = fixture.storage.raw().transaction()?;
    franken_batch_insert_new_messages(&write, fixture.conversation_id, &references)?;
    write.commit()?;
    let mut read = fixture.storage.raw().transaction()?;
    let mut streamed = Vec::new();
    visit_semantic_parent_identities(fixture.storage.raw(), |id, proof| {
        streamed.push((id, proof));
        Ok(())
    })?;
    assert_eq!(streamed.len(), 1026);
    assert!(streamed.windows(2).all(|pair| pair[0].0 < pair[1].0));
    let ids = streamed.iter().map(|(id, _)| *id).collect::<Vec<_>>();
    let looked_up = lookup_semantic_parent_identities_in_connection(fixture.storage.raw(), &ids)?;
    assert_eq!(looked_up, streamed.into_iter().collect());
    read.rollback()?;
    Ok(())
}

#[test]
fn gh458_supported_source_change_invalidates_affected_conversation_only() -> Result<()> {
    let fixture = Fixture::new()?;
    let before = fixture.state()?;
    let proof = fixture.proof()?;
    let mut source = fixture
        .storage
        .get_source(LOCAL_SOURCE_ID)?
        .context("local source")?;
    source.host_label = Some("replacement host label".to_owned());
    fixture.storage.upsert_source(&source)?;
    assert!(fixture.storage.semantic_invalidations_after(&before)?);
    assert_ne!(proof, fixture.proof()?);
    let changes = fixture
        .storage
        .semantic_conversation_changes_since(&before)?
        .context("delta")?;
    assert!(changes.complete);
    assert_eq!(changes.conversation_ids, [fixture.conversation_id]);
    fixture.storage.upsert_source(&source)?;
    assert_eq!(fixture.state()?, changes.state);
    Ok(())
}

#[test]
fn gh458_missing_journal_repair_retires_prior_archive_authority() -> Result<()> {
    let fixture = Fixture::new()?;
    let before = fixture.state()?;
    fixture.storage.set_last_embedded_message_id("fast", 1)?;
    fixture
        .storage
        .raw()
        .execute("DROP TABLE semantic_conversation_changes")?;
    fixture.storage.repair_missing_current_schema_objects()?;
    let repaired = fixture.state()?;
    assert_ne!(repaired.archive_id, before.archive_id);
    assert_eq!(repaired.mutation_seq, 0);
    assert!(
        fixture
            .storage
            .semantic_conversation_changes_since(&before)?
            .is_none()
    );
    assert!(fixture.storage.semantic_invalidations_after(&before)?);
    assert_eq!(fixture.storage.get_last_embedded_message_id("fast")?, None);
    Ok(())
}

#[test]
fn gh458_read_snapshot_retains_old_boundary_during_committed_append() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut transaction = fixture.storage.raw().transaction()?;
    let before = fixture.state()?;
    let path = fixture.storage.database_path()?;
    let agent = fixture.agent;
    let mut conversation = fixture.conversation.clone();
    conversation.messages.push(Fixture::message(1));
    let writer = std::thread::spawn(move || -> Result<()> {
        let storage = FrankenStorage::open(&path)?;
        storage.insert_conversation_tree_with_analytics(agent, None, &conversation, true)?;
        Ok(())
    });
    writer.join().expect("concurrent append thread")?;
    assert_eq!(fixture.state()?, before);
    let mut retained = Vec::new();
    visit_semantic_parent_identities(fixture.storage.raw(), |id, _| {
        retained.push(id);
        Ok(())
    })?;
    assert_eq!(retained, [1]);
    transaction.rollback()?;
    assert!(fixture.state()?.mutation_seq > before.mutation_seq);
    assert_eq!(fixture.storage.total_message_count()?, 2);
    Ok(())
}
