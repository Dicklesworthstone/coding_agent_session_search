// Included by indexer::tests. These tests use real canonical archives, native
// Quill segments and the production publication path. No mocked document counts
// or checkpoint-only model can establish append-safe recovery.

struct Gh496Fixture {
    _tmp: TempDir,
    data_dir: PathBuf,
    db_path: PathBuf,
    index_path: PathBuf,
    first_conversation_id: i64,
}

fn gh496_conversation(sequence: usize, messages: usize, body_bytes: usize) -> Conversation {
    let base = 1_700_000_000_000_i64 + sequence as i64 * 100_000;
    let sentence = "The lexical worker reads canonical messages, verifies stable document identities, \
                    and commits bounded batches. Interrupted archive indexing should retain durable \
                    progress while newly appended conversations become searchable. ";
    let body = sentence.repeat(body_bytes.div_ceil(sentence.len()));
    let body = &body[..body_bytes];
    Conversation {
        id: None,
        agent_slug: "codex".into(),
        workspace: Some(PathBuf::from("/workspace/gh496")),
        external_id: Some(format!("gh496-{sequence}")),
        title: Some(format!("Durable lexical fixture {sequence}")),
        source_path: PathBuf::from(format!("/archive/gh496-{sequence}.jsonl")),
        started_at: Some(base),
        ended_at: Some(base + messages as i64),
        approx_tokens: None,
        metadata_json: serde_json::Value::Null,
        messages: (0..messages)
            .map(|idx| Message {
                id: None,
                idx: idx as i64,
                role: MessageRole::User,
                author: Some("user".into()),
                created_at: Some(base + idx as i64),
                content: format!(
                    "growthproof stableword{sequence} messageword{idx} {body} finalword{sequence}z{idx}"
                ),
                extra_json: serde_json::Value::Null,
                snippets: Vec::new(),
            })
            .collect(),
        source_id: LOCAL_SOURCE_ID.into(),
        origin_host: None,
    }
}

fn gh496_insert_conversation(
    storage: &FrankenStorage,
    sequence: usize,
    messages: usize,
    body_bytes: usize,
) -> i64 {
    let agent_id = storage
        .ensure_agent(&Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: None,
            kind: AgentKind::Cli,
        })
        .unwrap();
    let workspace_id = storage
        .ensure_workspace(Path::new("/workspace/gh496"), None)
        .unwrap();
    storage
        .insert_conversation_tree(
            agent_id,
            Some(workspace_id),
            &gh496_conversation(sequence, messages, body_bytes),
        )
        .unwrap()
        .conversation_id
}

fn gh496_documents(db_path: &Path) -> Vec<frankensearch::quill::cass::CassDocument> {
    let storage = FrankenStorage::open_strict_readonly(db_path).unwrap();
    let (agents, workspaces) = storage.build_lexical_rebuild_lookups().unwrap();
    let mut documents = Vec::new();
    let mut after = 0;
    loop {
        let rows = storage
            .list_conversations_for_lexical_rebuild_after_id(64, after, &agents, &workspaces)
            .unwrap();
        if rows.is_empty() {
            break;
        }
        for row in rows {
            let id = row.id.unwrap();
            let messages = storage.fetch_messages_for_lexical_rebuild(id).unwrap();
            let (provenance, _) =
                lexical_rebuild_packet_provenance_from_canonical(&row, &HashMap::new());
            let packet =
                lexical_rebuild_contract_from_canonical_messages(&row, &provenance, messages);
            documents.extend(TantivyIndex::build_packet_documents(&packet, Some(id)));
            after = id;
        }
    }
    storage.close_without_checkpoint().unwrap();
    documents
}

fn gh496_stage_prefix(db_path: &Path, index_path: &Path, conversations: usize) {
    let storage = FrankenStorage::open_strict_readonly(db_path).unwrap();
    let (agents, workspaces) = storage.build_lexical_rebuild_lookups().unwrap();
    let rows = storage
        .list_conversations_for_lexical_rebuild_after_id(
            conversations as i64,
            0,
            &agents,
            &workspaces,
        )
        .unwrap();
    assert_eq!(rows.len(), conversations);
    let mut state = LexicalRebuildState::new(
        lexical_rebuild_db_state(&storage, db_path).unwrap(),
        LEXICAL_REBUILD_PAGE_SIZE,
    );
    let scratch = staged_lexical_rebuild_scratch_path(index_path);
    let mut writer = TantivyIndex::open_or_create(&scratch).unwrap();
    let mut indexed = 0;
    for row in &rows {
        let id = row.id.unwrap();
        let messages = storage.fetch_messages_for_lexical_rebuild(id).unwrap();
        let (provenance, _) =
            lexical_rebuild_packet_provenance_from_canonical(row, &HashMap::new());
        let packet = lexical_rebuild_contract_from_canonical_messages(row, &provenance, messages);
        indexed += writer
            .add_prebuilt_documents_slice(&TantivyIndex::build_packet_documents(&packet, Some(id)))
            .unwrap();
    }
    writer.commit().unwrap();
    drop(writer);
    storage.close_without_checkpoint().unwrap();
    state.resume_binding = lexical_resume::Binding::capture(db_path).unwrap();
    assert!(
        state.resume_binding.is_some(),
        "fixture requires archive identity support"
    );
    state.set_execution_mode(LexicalRebuildExecutionMode::StagedSingleIndex);
    state.record_pending_commit(rows.last().unwrap().id, conversations, indexed, None);
    state.finalize_commit(index_meta_fingerprint(&scratch).unwrap());
    persist_lexical_rebuild_state(index_path, &state).unwrap();
}

fn gh496_fixture() -> Gh496Fixture {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();
    let db_path = data_dir.join("agent_search.db");
    let storage = FrankenStorage::open(&db_path).unwrap();
    ensure_fts_schema(&storage);
    let first_conversation_id = gh496_insert_conversation(&storage, 0, 2, 32);
    gh496_insert_conversation(&storage, 1, 2, 32);
    drop(storage);
    rebuild_tantivy_from_db(&db_path, &data_dir, 2, None).unwrap();
    let index_path = index_dir(&data_dir).unwrap();
    gh496_stage_prefix(&db_path, &index_path, 1);
    Gh496Fixture {
        _tmp: tmp,
        data_dir,
        db_path,
        index_path,
        first_conversation_id,
    }
}

fn gh496_append(fixture: &Gh496Fixture) {
    let storage = FrankenStorage::open(&fixture.db_path).unwrap();
    // Replaying the same full conversation exercises ingestion's ordinary
    // append path, including an advanced ended_at and canonical tail caches.
    assert_eq!(
        gh496_insert_conversation(&storage, 0, 3, 32),
        fixture.first_conversation_id
    );
    gh496_insert_conversation(&storage, 2, 2, 32);
}

fn gh496_segments(path: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap())
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with("seg-") && name.ends_with(".fslx")
        })
        .map(|entry| {
            (
                PathBuf::from(entry.file_name()),
                fs::read(entry.path()).unwrap(),
            )
        })
        .collect()
}

fn gh496_assert_segments_retained(index_path: &Path, segments: &[(PathBuf, Vec<u8>)]) {
    assert!(
        !segments.is_empty(),
        "fixture must contain durable engine work"
    );
    for (name, bytes) in segments {
        assert_eq!(
            fs::read(index_path.join(name)).unwrap(),
            *bytes,
            "completed native segment {} was not retained",
            name.display()
        );
    }
}

fn gh496_assert_exact(fixture: &Gh496Fixture, expected_conversations: usize) {
    let documents = gh496_documents(&fixture.db_path);
    let snapshot =
        lexical_reconcile::canary::exact::PublishedSnapshot::open(&fixture.index_path).unwrap();
    assert_eq!(snapshot.doc_count() as usize, documents.len());
    for document in &documents {
        assert!(
            snapshot.verify_content(document).unwrap(),
            "missing or stale full document {:?}/{}",
            document.conversation_id,
            document.msg_idx
        );
    }
    let state = load_lexical_rebuild_state(&fixture.index_path)
        .unwrap()
        .unwrap();
    assert!(state.completed);
    assert!(state.pending.is_none());
    assert_eq!(state.processed_conversations, expected_conversations);
    assert_eq!(state.indexed_docs, documents.len());
    assert!(has_usable_lexical_publication_receipt(
        &fixture.index_path,
        &fixture.db_path
    ));
}

fn gh496_search_signature(index_path: &Path, query: &str) -> Vec<String> {
    use crate::search::query::{FieldMask, SearchClient, SearchFilters};
    let client = SearchClient::open(index_path, None).unwrap().unwrap();
    let hits = client
        .search(query, SearchFilters::default(), 1000, 0, FieldMask::FULL)
        .unwrap();
    let mut values = hits
        .into_iter()
        .map(|hit| {
            serde_json::json!({
                "conversation_id": hit.conversation_id,
                "message_index": hit.line_number,
                "source_id": hit.source_id,
                "source_path": hit.source_path,
                "agent": hit.agent,
                "workspace": hit.workspace,
                "title": hit.title,
                "created_at": hit.created_at,
                "content": hit.content,
            })
            .to_string()
        })
        .collect::<Vec<_>>();
    values.sort();
    values
}

#[test]
#[serial_test::serial]
fn gh496_append_resume_retains_native_work_and_matches_fresh_search() {
    let fixture = gh496_fixture();
    let scratch = staged_lexical_rebuild_scratch_path(&fixture.index_path);
    let segments = gh496_segments(&scratch);
    gh496_append(&fixture);
    let outcome = rebuild_tantivy_from_db(&fixture.db_path, &fixture.data_dir, 3, None).unwrap();
    assert_eq!(outcome.observed_conversations, 3);
    assert_eq!(outcome.indexed_docs, 7);
    gh496_assert_segments_retained(&fixture.index_path, &segments);
    gh496_assert_exact(&fixture, 3);

    let reference_dir = fixture._tmp.path().join("from-zero");
    fs::create_dir_all(&reference_dir).unwrap();
    rebuild_tantivy_from_db(&fixture.db_path, &reference_dir, 3, None).unwrap();
    let reference_index = index_dir(&reference_dir).unwrap();
    for query in ["growthproof", "stableword0", "stableword2", "finalword0z2"] {
        assert_eq!(
            gh496_search_signature(&fixture.index_path, query),
            gh496_search_signature(&reference_index, query),
            "resumption and a fresh generation disagree for {query}"
        );
    }
}

#[test]
#[serial_test::serial]
fn gh496_existing_unbound_staged_checkpoint_is_adopted_only_through_full_audit() {
    let fixture = gh496_fixture();
    let segments = gh496_segments(&staged_lexical_rebuild_scratch_path(&fixture.index_path));
    let mut state = load_lexical_rebuild_state(&fixture.index_path)
        .unwrap()
        .unwrap();
    state.resume_binding = None;
    persist_lexical_rebuild_state(&fixture.index_path, &state).unwrap();
    gh496_append(&fixture);
    rebuild_tantivy_from_db(&fixture.db_path, &fixture.data_dir, 3, None).unwrap();
    gh496_assert_segments_retained(&fixture.index_path, &segments);
    gh496_assert_exact(&fixture, 3);
    let completed = load_lexical_rebuild_state(&fixture.index_path)
        .unwrap()
        .unwrap();
    assert!(
        completed.resume_binding.is_some(),
        "successful adoption must persist its archive binding"
    );

    // A missing mode cannot establish that the checkpoint cursor describes
    // the scratch sibling, so it is not a candidate for this new audit lane.
    state.execution_mode = None;
    assert!(!lexical_resume::eligible(&state, &fixture.db_path));
    state.execution_mode = Some(LexicalRebuildExecutionMode::SharedWriter);
    assert!(!lexical_resume::eligible(&state, &fixture.db_path));
}

#[test]
#[serial_test::serial]
fn gh496_legacy_live_cursors_cannot_skip_same_count_prefix_edits() {
    for mode in [
        LexicalRebuildExecutionMode::SharedWriter,
        LexicalRebuildExecutionMode::LiveSingleIndex,
    ] {
        for bound in [false, true] {
            let fixture = gh496_fixture();
            let mut state = load_lexical_rebuild_state(&fixture.index_path)
                .unwrap()
                .unwrap();
            let scratch = staged_lexical_rebuild_scratch_path(&fixture.index_path);
            fs::rename(
                &fixture.index_path,
                fixture._tmp.path().join("preserved-prior-live"),
            )
            .unwrap();
            // Model a legacy in-place run that committed only conversation 1.
            // Its cursor and fingerprint are real, but cannot prove that the
            // retained prefix stayed unchanged between attempts.
            fs::rename(&scratch, &fixture.index_path).unwrap();
            state.set_execution_mode(mode);
            if !bound {
                state.resume_binding = None;
            }
            persist_lexical_rebuild_state(&fixture.index_path, &state).unwrap();
            assert_eq!(state.processed_conversations, 1);
            assert_eq!(state.indexed_docs, 2);
            assert!(!lexical_resume::eligible(&state, &fixture.db_path));

            let storage = FrankenStorage::open(&fixture.db_path).unwrap();
            let db_before = lexical_rebuild_db_state(&storage, &fixture.db_path).unwrap();
            storage
                .raw()
                .execute(&format!(
                    "UPDATE messages SET content = 'growthproof rewrittenlegacyprefix' \
                     WHERE conversation_id = {} AND idx = 0",
                    fixture.first_conversation_id,
                ))
                .unwrap();
            assert_eq!(
                lexical_rebuild_db_state(&storage, &fixture.db_path).unwrap(),
                db_before,
                "the legacy count/ID fingerprint cannot detect this prefix edit"
            );
            storage.close_without_checkpoint().unwrap();

            rebuild_tantivy_from_db(&fixture.db_path, &fixture.data_dir, 2, None).unwrap();
            gh496_assert_exact(&fixture, 2);
            assert!(gh496_search_signature(&fixture.index_path, "finalword0z0").is_empty());
            assert_eq!(
                gh496_search_signature(&fixture.index_path, "rewrittenlegacyprefix").len(),
                1,
                "mode={} bound={bound} must rebuild the edited prefix",
                mode.as_str(),
            );
        }
    }
}

#[test]
#[serial_test::serial]
fn gh496_full_index_preflight_preserves_a_growth_eligible_stage() {
    let fixture = gh496_fixture();
    let segments = gh496_segments(&staged_lexical_rebuild_scratch_path(&fixture.index_path));
    gh496_append(&fixture);
    // Exercise the user-facing --full startup/pre-scan path. The closed set of
    // connector roots prevents host sessions from contaminating the fixture.
    run_index_with_local_connector_roots(
        IndexOptions {
            full: true,
            force_rebuild: false,
            watch: false,
            watch_once_paths: None,
            db_path: fixture.db_path.clone(),
            data_dir: fixture.data_dir.clone(),
            semantic: false,
            build_hnsw: false,
            embedder: "hash".into(),
            progress: Some(Arc::new(IndexingProgress::default())),
            watch_interval_secs: 30,
        },
        HashMap::new(),
        None,
    )
    .unwrap();
    gh496_assert_segments_retained(&fixture.index_path, &segments);
    gh496_assert_exact(&fixture, 3);
}

#[test]
#[serial_test::serial]
fn gh496_repeated_interruptions_preserve_committed_delta_without_duplicates() {
    let fixture = gh496_fixture();
    let live_manifest = fs::read(fixture.index_path.join("MANIFEST")).unwrap();
    gh496_append(&fixture);
    for attempt in 0..2 {
        if attempt != 0 {
            let storage = FrankenStorage::open(&fixture.db_path).unwrap();
            gh496_insert_conversation(&storage, 3, 2, 32);
        }
        let state = load_lexical_rebuild_state(&fixture.index_path)
            .unwrap()
            .unwrap();
        let mut committed = false;
        let error = lexical_resume::run_with_observer(
            &fixture.db_path,
            &fixture.data_dir,
            &fixture.index_path,
            state,
            None,
            None,
            |event| {
                if let lexical_resume::Event::CommittedBatch { .. } = event {
                    committed = true;
                    anyhow::bail!("GH496 interruption after durable engine commit");
                }
                Ok(())
            },
        )
        .expect_err("injected interruption must stop publication");
        assert!(committed);
        assert!(format!("{error:#}").contains("GH496 interruption"));
        assert_eq!(
            fs::read(fixture.index_path.join("MANIFEST")).unwrap(),
            live_manifest
        );
        let saved = load_lexical_rebuild_state(&fixture.index_path)
            .unwrap()
            .unwrap();
        assert!(!saved.completed);
        assert_eq!(
            saved.effective_execution_mode(),
            LexicalRebuildExecutionMode::StagedVerifiedReplay
        );
        let scratch = staged_lexical_rebuild_scratch_path(&fixture.index_path);
        let native = lexical_reconcile::canary::exact::PublishedSnapshot::open(&scratch).unwrap();
        assert!(
            native.doc_count() > 2,
            "new work must be durable before interruption"
        );
    }
    rebuild_tantivy_from_db(&fixture.db_path, &fixture.data_dir, 4, None).unwrap();
    gh496_assert_exact(&fixture, 4);
    let first = gh496_search_signature(&fixture.index_path, "growthproof");
    rebuild_tantivy_from_db(&fixture.db_path, &fixture.data_dir, 4, None).unwrap();
    assert_eq!(
        gh496_search_signature(&fixture.index_path, "growthproof"),
        first
    );
    gh496_assert_exact(&fixture, 4);
}

#[test]
#[serial_test::serial]
fn gh496_canonical_writer_during_audit_cannot_certify_an_incomplete_generation() {
    let fixture = gh496_fixture();
    gh496_append(&fixture);
    let live_manifest = fs::read(fixture.index_path.join("MANIFEST")).unwrap();
    let writer = FrankenStorage::open(&fixture.db_path).unwrap();
    let mut changed = false;
    let state = load_lexical_rebuild_state(&fixture.index_path)
        .unwrap()
        .unwrap();
    let error = lexical_resume::run_with_observer(
        &fixture.db_path,
        &fixture.data_dir,
        &fixture.index_path,
        state,
        None,
        None,
        |event| {
            if let lexical_resume::Event::BeforePublication = event {
                gh496_insert_conversation(&writer, 3, 2, 32);
                changed = true;
            }
            Ok(())
        },
    )
    .expect_err("a write after verification must invalidate publication");
    assert!(changed, "the real canonical writer did not run: {error:#}");
    assert!(error.is::<lexical_resume::SourceChanged>(), "{error:#}");
    drop(writer);
    assert_eq!(
        fs::read(fixture.index_path.join("MANIFEST")).unwrap(),
        live_manifest
    );
    assert!(
        !load_lexical_rebuild_state(&fixture.index_path)
            .unwrap()
            .unwrap()
            .completed
    );
    assert!(staged_lexical_rebuild_scratch_path(&fixture.index_path).exists());
    rebuild_tantivy_from_db(&fixture.db_path, &fixture.data_dir, 4, None).unwrap();
    gh496_assert_exact(&fixture, 4);
}

#[test]
#[serial_test::serial]
fn gh496_publication_failure_then_growth_rechecks_the_completed_candidate() {
    #[cfg(windows)]
    const DISK_FULL: i32 = 112;
    #[cfg(not(windows))]
    const DISK_FULL: i32 = libc::ENOSPC;
    let fixture = gh496_fixture();
    gh496_append(&fixture);
    let live_manifest = fs::read(fixture.index_path.join("MANIFEST")).unwrap();
    #[cfg(target_os = "linux")]
    let fault = inject_lexical_publish_rename_failure_once(
        LexicalPublishRenameSite::LinuxParkPriorLiveToCanonicalSidecar,
        DISK_FULL,
    );
    #[cfg(not(target_os = "linux"))]
    let fault = inject_lexical_publish_rename_failure_once(
        LexicalPublishRenameSite::NonLinuxPublishStagedLive,
        DISK_FULL,
    );
    rebuild_tantivy_from_db(&fixture.db_path, &fixture.data_dir, 3, None)
        .expect_err("actual atomic publication must fail at the injected rename");
    drop(fault);
    assert_eq!(
        fs::read(fixture.index_path.join("MANIFEST")).unwrap(),
        live_manifest
    );
    assert!(
        !load_lexical_rebuild_state(&fixture.index_path)
            .unwrap()
            .unwrap()
            .completed
    );
    let scratch = staged_lexical_rebuild_scratch_path(&fixture.index_path);
    let candidate = lexical_reconcile::canary::exact::PublishedSnapshot::open(&scratch).unwrap();
    assert_eq!(
        candidate.doc_count(),
        7,
        "a failed publication must retain completed native work"
    );
    drop(candidate);
    let storage = FrankenStorage::open(&fixture.db_path).unwrap();
    gh496_insert_conversation(&storage, 3, 2, 32);
    drop(storage);
    rebuild_tantivy_from_db(&fixture.db_path, &fixture.data_dir, 4, None).unwrap();
    gh496_assert_exact(&fixture, 4);
}

#[test]
#[serial_test::serial]
fn gh496_first_publish_failure_recovers_scratch_only_checkpoint_after_growth() {
    #[cfg(windows)]
    const DISK_FULL: i32 = 112;
    #[cfg(not(windows))]
    const DISK_FULL: i32 = libc::ENOSPC;
    let fixture = gh496_fixture();
    let state = load_lexical_rebuild_state(&fixture.index_path)
        .unwrap()
        .unwrap();
    // A first build has no prior published generation: only its progress
    // checkpoint occupies the live placeholder. Preserve the fixture's old
    // publication elsewhere so the real first-publish path is exercised.
    fs::rename(
        &fixture.index_path,
        fixture._tmp.path().join("prior-publication-held"),
    )
    .unwrap();
    fs::create_dir_all(&fixture.index_path).unwrap();
    persist_lexical_rebuild_state(&fixture.index_path, &state).unwrap();
    gh496_append(&fixture);
    let fault = inject_lexical_publish_rename_failure_once(
        LexicalPublishRenameSite::FirstPublish,
        DISK_FULL,
    );
    rebuild_tantivy_from_db(&fixture.db_path, &fixture.data_dir, 3, None)
        .expect_err("first rename fails after removing the checkpoint-only placeholder");
    drop(fault);
    assert!(!lexical_rebuild_state_path(&fixture.index_path).exists());
    let scratch = staged_lexical_rebuild_scratch_path(&fixture.index_path);
    let candidate = lexical_reconcile::canary::exact::PublishedSnapshot::open(&scratch).unwrap();
    assert_eq!(candidate.doc_count(), 7);
    drop(candidate);
    let terminal = load_lexical_rebuild_state(&scratch).unwrap().unwrap();
    assert!(terminal.completed);
    let segments = gh496_segments(&scratch);
    let storage = FrankenStorage::open(&fixture.db_path).unwrap();
    gh496_insert_conversation(&storage, 3, 2, 32);
    drop(storage);
    rebuild_tantivy_from_db(&fixture.db_path, &fixture.data_dir, 4, None).unwrap();
    gh496_assert_segments_retained(&fixture.index_path, &segments);
    gh496_assert_exact(&fixture, 4);
}

#[test]
#[serial_test::serial]
fn gh496_edits_deletions_and_identity_changes_refuse_prefix_reuse() {
    for mutation in [
        "UPDATE messages SET content = 'growthproof replacementbody' WHERE conversation_id = 1 AND idx = 0",
        "DELETE FROM messages WHERE conversation_id = 1 AND idx = 0",
        "UPDATE messages SET idx = 9 WHERE conversation_id = 1 AND idx = 0",
        "UPDATE messages SET created_at = created_at + 1 WHERE conversation_id = 1 AND idx = 0",
        "UPDATE conversations SET title = 'Changed indexed title' WHERE id = 1",
        "UPDATE conversations SET source_path = '/archive/replaced.jsonl' WHERE id = 1",
        "UPDATE conversations SET source_id = 'other-source' WHERE id = 1",
        "UPDATE conversations SET origin_host = 'changed-host' WHERE id = 1",
        "UPDATE workspaces SET path = '/workspace/changed' WHERE path = '/workspace/gh496'",
        "UPDATE agents SET slug = 'changed-agent' WHERE slug = 'codex'",
    ] {
        let fixture = gh496_fixture();
        assert_eq!(fixture.first_conversation_id, 1);
        let live_manifest = fs::read(fixture.index_path.join("MANIFEST")).unwrap();
        let storage = FrankenStorage::open(&fixture.db_path).unwrap();
        storage
            .raw()
            .execute("INSERT INTO sources (id, kind, created_at, updated_at) VALUES ('other-source', 'ssh', 1, 1)")
            .unwrap();
        storage.raw().execute(mutation).unwrap();
        drop(storage);
        let state = load_lexical_rebuild_state(&fixture.index_path)
            .unwrap()
            .unwrap();
        let error = lexical_resume::run(
            &fixture.db_path,
            &fixture.data_dir,
            &fixture.index_path,
            state,
            None,
            None,
        )
        .expect_err(mutation);
        assert!(
            error.is::<lexical_resume::Incompatible>(),
            "{mutation}: {error:#}"
        );
        assert_eq!(
            fs::read(fixture.index_path.join("MANIFEST")).unwrap(),
            live_manifest,
            "{mutation}"
        );
        assert!(
            !load_lexical_rebuild_state(&fixture.index_path)
                .unwrap()
                .unwrap()
                .completed
        );
        // The public entry point may quarantine incompatible work and rebuild;
        // success must then agree with every current canonical projection.
        rebuild_tantivy_from_db(&fixture.db_path, &fixture.data_dir, 2, None).unwrap();
        gh496_assert_exact(&fixture, 2);
    }
}

#[test]
#[serial_test::serial]
fn gh496_same_length_content_rewrite_beyond_preview_is_not_append_growth() {
    let fixture = gh496_fixture();
    let storage = FrankenStorage::open(&fixture.db_path).unwrap();
    let long = format!("{} oldtailword", "prefix ".repeat(1024));
    storage
        .raw()
        .execute(&format!(
            "UPDATE messages SET content = '{long}' WHERE conversation_id = 1 AND idx = 0"
        ))
        .unwrap();
    drop(storage);
    // Preserve the initial stage for inspection, then seed a stage containing
    // a long full document whose indexed tail is absent from stored previews.
    let scratch = staged_lexical_rebuild_scratch_path(&fixture.index_path);
    fs::rename(&scratch, fixture._tmp.path().join("initial-stage")).unwrap();
    gh496_stage_prefix(&fixture.db_path, &fixture.index_path, 1);
    let storage = FrankenStorage::open(&fixture.db_path).unwrap();
    let before = lexical_rebuild_db_state(&storage, &fixture.db_path).unwrap();
    storage
        .raw()
        .execute(&format!(
            "UPDATE messages SET content = '{}' WHERE conversation_id = 1 AND idx = 0",
            long.replace("oldtailword", "newtailword")
        ))
        .unwrap();
    let after = lexical_rebuild_db_state(&storage, &fixture.db_path).unwrap();
    assert_eq!(
        before.storage_fingerprint, after.storage_fingerprint,
        "cheap counts/ids intentionally cannot see this rewrite"
    );
    drop(storage);
    let state = load_lexical_rebuild_state(&fixture.index_path)
        .unwrap()
        .unwrap();
    assert!(
        lexical_resume::run(
            &fixture.db_path,
            &fixture.data_dir,
            &fixture.index_path,
            state,
            None,
            None
        )
        .is_err()
    );
    rebuild_tantivy_from_db(&fixture.db_path, &fixture.data_dir, 2, None).unwrap();
    gh496_assert_exact(&fixture, 2);
    assert!(gh496_search_signature(&fixture.index_path, "oldtailword").is_empty());
    assert_eq!(
        gh496_search_signature(&fixture.index_path, "newtailword").len(),
        1
    );
}

#[test]
#[serial_test::serial]
fn gh496_completed_recovery_cannot_certify_later_same_count_edits() {
    let fixture = gh496_fixture();
    gh496_append(&fixture);
    rebuild_tantivy_from_db(&fixture.db_path, &fixture.data_dir, 3, None).unwrap();
    gh496_assert_exact(&fixture, 3);
    let completed = load_lexical_rebuild_state(&fixture.index_path)
        .unwrap()
        .unwrap();
    assert!(completed.resume_binding.is_some());

    let storage = FrankenStorage::open(&fixture.db_path).unwrap();
    let before = lexical_rebuild_db_state(&storage, &fixture.db_path).unwrap();
    storage
        .raw()
        .execute(
            "UPDATE messages SET content = 'growthproof rewrittenaftercompletion' \
         WHERE conversation_id = 1 AND idx = 0",
        )
        .unwrap();
    let after = lexical_rebuild_db_state(&storage, &fixture.db_path).unwrap();
    assert_eq!(
        before, after,
        "the cheap count/ID fingerprint must miss this edit"
    );
    assert!(!completed.matches_run(&after, LEXICAL_REBUILD_PAGE_SIZE));
    assert!(!published_lexical_index_validated_for_current_data(
        &fixture.index_path,
        &fixture.db_path
    ));

    // A metadata/count refresh must not turn the stale content into current
    // publication authority merely because the native document count agrees.
    refresh_completed_lexical_rebuild_checkpoint(&storage, &fixture.db_path, &fixture.data_dir)
        .unwrap();
    let refreshed = load_lexical_rebuild_state(&fixture.index_path)
        .unwrap()
        .unwrap();
    assert!(!refreshed.matches_run(&after, LEXICAL_REBUILD_PAGE_SIZE));
    assert_eq!(
        refreshed.completed_source_epoch,
        completed.completed_source_epoch
    );
    assert!(!published_lexical_index_validated_for_current_data(
        &fixture.index_path,
        &fixture.db_path
    ));
    assert!(
        !matching_completed_lexical_rebuild_state_status_without_fingerprint(
            &fixture.index_path,
            &fixture.db_path,
            3,
        )
        .unwrap()
        .unwrap()
        .has_completed_checkpoint
    );
    drop(storage);

    rebuild_tantivy_from_db(&fixture.db_path, &fixture.data_dir, 3, None).unwrap();
    gh496_assert_exact(&fixture, 3);
    assert!(gh496_search_signature(&fixture.index_path, "finalword0z0").is_empty());
    assert_eq!(
        gh496_search_signature(&fixture.index_path, "rewrittenaftercompletion").len(),
        1
    );
}

#[test]
#[serial_test::serial]
fn gh496_flipped_completed_bit_on_a_bound_prefix_cannot_hide_missing_rows() {
    let fixture = gh496_fixture();
    let mut state = load_lexical_rebuild_state(&fixture.index_path)
        .unwrap()
        .unwrap();
    let documents = gh496_documents(&fixture.db_path);
    // Both the old live generation and the staged prefix have two documents.
    // This makes native count equality alone unable to distinguish a corrupt
    // completed flag from a truly complete four-document canonical rebuild.
    fs::rename(
        &fixture.index_path,
        fixture._tmp.path().join("preserved-live"),
    )
    .unwrap();
    let mut live = TantivyIndex::open_or_create(&fixture.index_path).unwrap();
    live.add_prebuilt_documents_slice(&documents[..2]).unwrap();
    live.commit().unwrap();
    drop(live);
    state.completed = true;
    state.pending = None;
    persist_lexical_rebuild_state(&fixture.index_path, &state).unwrap();
    rebuild_tantivy_from_db(&fixture.db_path, &fixture.data_dir, 2, None).unwrap();
    gh496_assert_exact(&fixture, 2);
    assert_eq!(
        gh496_search_signature(&fixture.index_path, "growthproof").len(),
        4
    );
}

#[test]
#[serial_test::serial]
fn gh496_corrupt_checkpoint_counters_do_not_certify_missing_documents() {
    for field in [
        "committed_offset",
        "processed_conversations",
        "indexed_docs",
    ] {
        let fixture = gh496_fixture();
        gh496_append(&fixture);
        let path = lexical_rebuild_state_path(&fixture.index_path);
        let mut state: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        state[field] = serde_json::json!(999999);
        write_json_pretty_atomically(&path, &state).unwrap();
        rebuild_tantivy_from_db(&fixture.db_path, &fixture.data_dir, 3, None).unwrap();
        gh496_assert_exact(&fixture, 3);
    }
}

#[test]
#[serial_test::serial]
fn gh496_corrupt_stage_manifest_never_replaces_live_generation() {
    let fixture = gh496_fixture();
    gh496_append(&fixture);
    let live_manifest = fs::read(fixture.index_path.join("MANIFEST")).unwrap();
    let scratch = staged_lexical_rebuild_scratch_path(&fixture.index_path);
    fs::write(scratch.join("MANIFEST"), b"not a native manifest").unwrap();
    let state = load_lexical_rebuild_state(&fixture.index_path)
        .unwrap()
        .unwrap();
    assert!(
        lexical_resume::run(
            &fixture.db_path,
            &fixture.data_dir,
            &fixture.index_path,
            state,
            None,
            None
        )
        .is_err()
    );
    assert_eq!(
        fs::read(fixture.index_path.join("MANIFEST")).unwrap(),
        live_manifest
    );
    assert!(
        !load_lexical_rebuild_state(&fixture.index_path)
            .unwrap()
            .unwrap()
            .completed
    );
}

#[test]
#[serial_test::serial]
fn gh496_temporarily_missing_archive_preserves_candidate_for_growth_recovery() {
    let fixture = gh496_fixture();
    let state = load_lexical_rebuild_state(&fixture.index_path)
        .unwrap()
        .unwrap();
    assert!(state.resume_binding.is_some());
    assert!(lexical_resume::eligible(&state, &fixture.db_path));
    let checkpoint = fs::read(lexical_rebuild_state_path(&fixture.index_path)).unwrap();
    let scratch = staged_lexical_rebuild_scratch_path(&fixture.index_path);
    let staged_manifest = fs::read(scratch.join("MANIFEST")).unwrap();
    let segments = gh496_segments(&scratch);
    let preserved_db = fixture
        ._tmp
        .path()
        .join("temporarily-unavailable-archive.db");

    // Keep the same native file identity for restoration. A temporary source
    // failure is not evidence that committed staged documents are incompatible.
    fs::rename(&fixture.db_path, &preserved_db).unwrap();
    let error = rebuild_tantivy_from_db(&fixture.db_path, &fixture.data_dir, 2, None)
        .expect_err("an unavailable archive must defer recovery without resetting it");
    assert!(!error.is::<lexical_resume::Incompatible>(), "{error:#}");
    assert_eq!(
        fs::read(lexical_rebuild_state_path(&fixture.index_path)).unwrap(),
        checkpoint
    );
    assert_eq!(fs::read(scratch.join("MANIFEST")).unwrap(), staged_manifest);
    gh496_assert_segments_retained(&scratch, &segments);
    assert!(
        !fs::read_dir(fixture.index_path.parent().unwrap())
            .unwrap()
            .map(Result::unwrap)
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".lexical-rebuild-quarantine-")
            }),
        "temporary source unavailability must not quarantine valid completed work"
    );

    fs::rename(&preserved_db, &fixture.db_path).unwrap();
    assert!(lexical_resume::eligible(&state, &fixture.db_path));
    gh496_append(&fixture);
    let outcome = rebuild_tantivy_from_db(&fixture.db_path, &fixture.data_dir, 3, None).unwrap();
    assert_eq!(outcome.indexed_docs, 7);
    gh496_assert_segments_retained(&fixture.index_path, &segments);
    gh496_assert_exact(&fixture, 3);
}

#[test]
#[serial_test::serial]
fn gh496_replaced_archive_at_same_path_is_not_the_checkpoint_archive() {
    let fixture = gh496_fixture();
    let state = load_lexical_rebuild_state(&fixture.index_path)
        .unwrap()
        .unwrap();
    assert!(lexical_resume::eligible(&state, &fixture.db_path));
    let preserved = fixture._tmp.path().join("old-archive");
    fs::create_dir_all(&preserved).unwrap();
    for suffix in ["", "-wal", "-shm"] {
        let source = PathBuf::from(format!("{}{suffix}", fixture.db_path.display()));
        if source.exists() {
            fs::rename(&source, preserved.join(source.file_name().unwrap())).unwrap();
        }
    }
    let replacement = FrankenStorage::open(&fixture.db_path).unwrap();
    ensure_fts_schema(&replacement);
    gh496_insert_conversation(&replacement, 0, 2, 32);
    gh496_insert_conversation(&replacement, 1, 2, 32);
    drop(replacement);
    assert!(!lexical_resume::eligible(&state, &fixture.db_path));
    rebuild_tantivy_from_db(&fixture.db_path, &fixture.data_dir, 2, None).unwrap();
    gh496_assert_exact(&fixture, 2);
}

#[test]
#[serial_test::serial]
fn gh496_content_cap_or_schema_drift_rejects_resume_binding() {
    let _cap = set_env_var("CASS_LEXICAL_MAX_MESSAGE_CONTENT_BYTES", "8192");
    let fixture = gh496_fixture();
    let mut state = load_lexical_rebuild_state(&fixture.index_path)
        .unwrap()
        .unwrap();
    assert!(lexical_resume::eligible(&state, &fixture.db_path));
    state.schema_hash.push_str("-incompatible");
    assert!(!lexical_resume::eligible(&state, &fixture.db_path));
    let state = load_lexical_rebuild_state(&fixture.index_path)
        .unwrap()
        .unwrap();
    let _changed_cap = set_env_var("CASS_LEXICAL_MAX_MESSAGE_CONTENT_BYTES", "4096");
    assert!(!lexical_resume::eligible(&state, &fixture.db_path));
    rebuild_tantivy_from_db(&fixture.db_path, &fixture.data_dir, 2, None).unwrap();
    gh496_assert_exact(&fixture, 2);
}

#[test]
#[serial_test::serial]
fn gh496_pending_engine_commit_is_reaudited_after_recovery() {
    let fixture = gh496_fixture();
    let scratch = staged_lexical_rebuild_scratch_path(&fixture.index_path);
    let all_documents = gh496_documents(&fixture.db_path);
    let mut state = load_lexical_rebuild_state(&fixture.index_path)
        .unwrap()
        .unwrap();
    state.record_pending_commit(Some(2), 2, 4, index_meta_fingerprint(&scratch).unwrap());
    persist_lexical_rebuild_state(&fixture.index_path, &state).unwrap();
    let mut writer = TantivyIndex::open_or_create(&scratch).unwrap();
    writer
        .add_prebuilt_documents_slice(&all_documents[2..])
        .unwrap();
    writer.commit().unwrap();
    drop(writer);
    // Model process loss after the durable native commit, before finalizing
    // the JSON checkpoint. New ingestion is allowed before the next attempt.
    gh496_append(&fixture);
    rebuild_tantivy_from_db(&fixture.db_path, &fixture.data_dir, 3, None).unwrap();
    gh496_assert_exact(&fixture, 3);
    assert_eq!(
        gh496_search_signature(&fixture.index_path, "growthproof").len(),
        7
    );
}

#[test]
#[serial_test::serial]
fn gh496_bounded_batches_checkpoint_inside_a_growing_conversation() {
    let fixture = gh496_fixture();
    let _initial = set_env_var("CASS_TANTIVY_REBUILD_INITIAL_COMMIT_EVERY_MESSAGES", "8");
    let _steady = set_env_var("CASS_TANTIVY_REBUILD_COMMIT_EVERY_MESSAGES", "8");
    let _bytes = set_env_var(
        "CASS_TANTIVY_REBUILD_PIPELINE_MAX_MESSAGE_BYTES_IN_FLIGHT",
        "2048",
    );
    let storage = FrankenStorage::open(&fixture.db_path).unwrap();
    gh496_insert_conversation(&storage, 0, 64, 32);
    drop(storage);
    let state = load_lexical_rebuild_state(&fixture.index_path)
        .unwrap()
        .unwrap();
    let mut commits = 0;
    let mut last_docs = 2;
    let error = lexical_resume::run_with_observer(
        &fixture.db_path,
        &fixture.data_dir,
        &fixture.index_path,
        state,
        None,
        None,
        |event| {
            if let lexical_resume::Event::CommittedBatch { indexed_docs } = event {
                assert!(indexed_docs > last_docs);
                assert!(
                    indexed_docs - last_docs <= 8,
                    "commit exceeded the configured message budget"
                );
                last_docs = indexed_docs;
                commits += 1;
                if commits == 2 {
                    anyhow::bail!("GH496 interrupt within long conversation");
                }
            }
            Ok(())
        },
    )
    .expect_err("interrupted inside a conversation");
    assert!(format!("{error:#}").contains("within long conversation"));
    let state = load_lexical_rebuild_state(&fixture.index_path)
        .unwrap()
        .unwrap();
    assert_eq!(
        state.processed_conversations, 0,
        "a whole conversation was not yet consumed"
    );
    assert_eq!(state.indexed_docs, last_docs);
    let storage = FrankenStorage::open(&fixture.db_path).unwrap();
    gh496_insert_conversation(&storage, 0, 68, 32);
    drop(storage);
    // Memory settings may change without changing lexical projection. The
    // next attempt must verify the durable partial conversation all over again.
    let _smaller = set_env_var(
        "CASS_TANTIVY_REBUILD_PIPELINE_MAX_MESSAGE_BYTES_IN_FLIGHT",
        "1024",
    );
    assert!(lexical_resume::eligible(&state, &fixture.db_path));
    rebuild_tantivy_from_db(&fixture.db_path, &fixture.data_dir, 2, None).unwrap();
    gh496_assert_exact(&fixture, 2);
    assert_eq!(
        gh496_search_signature(&fixture.index_path, "growthproof").len(),
        70
    );
}

#[test]
#[serial_test::serial]
fn gh496_truncated_checkpoint_never_authorizes_publication() {
    let fixture = gh496_fixture();
    gh496_append(&fixture);
    let manifest = fs::read(fixture.index_path.join("MANIFEST")).unwrap();
    fs::write(
        lexical_rebuild_state_path(&fixture.index_path),
        b"{\"version\":",
    )
    .unwrap();
    let result = rebuild_tantivy_from_db(&fixture.db_path, &fixture.data_dir, 3, None);
    match result {
        Ok(_) => gh496_assert_exact(&fixture, 3),
        Err(_) => {
            assert_eq!(
                fs::read(fixture.index_path.join("MANIFEST")).unwrap(),
                manifest
            );
            assert_eq!(
                gh496_search_signature(&fixture.index_path, "growthproof").len(),
                4
            );
        }
    }
}

fn gh496_benchmark_size(name: &str, fallback: usize) -> usize {
    dotenvy::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(fallback)
}

/// Run both competitors in the same driver invocation, each in a fresh child
/// process against the SAME immutable canonical database. Worker VmHWM includes
/// only that recovery/restart process, not fixture generation or compilation.
#[test]
#[ignore = "real interrupted-resume versus restart benchmark; run explicitly with --ignored --nocapture"]
#[serial_test::serial]
fn gh496_benchmark_interrupted_resume_against_restart() {
    let conversations = gh496_benchmark_size("CASS_GH496_BENCH_CONVERSATIONS", 200).max(4);
    let messages = gh496_benchmark_size("CASS_GH496_BENCH_MESSAGES", 40).max(1);
    let body_bytes = gh496_benchmark_size("CASS_GH496_BENCH_BODY_BYTES", 1024);
    let completed_conversations = conversations * 3 / 4;
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("benchmark.db");
    let storage = FrankenStorage::open(&db_path).unwrap();
    ensure_fts_schema(&storage);
    for sequence in 0..conversations {
        gh496_insert_conversation(&storage, sequence, messages, body_bytes);
    }
    drop(storage);
    let resume_dir = tmp.path().join("resume");
    fs::create_dir_all(&resume_dir).unwrap();
    let index_path = index_dir(&resume_dir).unwrap();
    fs::create_dir_all(&index_path).unwrap();
    gh496_stage_prefix(&db_path, &index_path, completed_conversations);
    let storage = FrankenStorage::open(&db_path).unwrap();
    gh496_insert_conversation(&storage, 0, messages + 1, body_bytes);
    for sequence in conversations..conversations + conversations / 10 {
        gh496_insert_conversation(&storage, sequence, messages, body_bytes);
    }
    drop(storage);
    let total_conversations = conversations + conversations / 10;
    let expected_docs = total_conversations * messages + 1;
    let restart_dir = tmp.path().join("restart");
    fs::create_dir_all(&restart_dir).unwrap();
    for (mode, data_dir) in [("resume", &resume_dir), ("restart", &restart_dir)] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "indexer::tests::gh496_benchmark_worker",
                "--ignored",
                "--nocapture",
            ])
            .env("CASS_GH496_BENCH_MODE", mode)
            .env("CASS_GH496_BENCH_DB", &db_path)
            .env("CASS_GH496_BENCH_DATA", data_dir)
            .env("CASS_GH496_BENCH_EXPECTED_DOCS", expected_docs.to_string())
            .env(
                "CASS_GH496_BENCH_EXPECTED_CONVERSATIONS",
                total_conversations.to_string(),
            )
            .env(
                "CASS_GH496_BENCH_REUSED_DOCS",
                (completed_conversations * messages).to_string(),
            )
            .output()
            .unwrap();
        eprintln!("{}", String::from_utf8_lossy(&output.stdout));
        eprintln!("{}", String::from_utf8_lossy(&output.stderr));
        assert!(output.status.success(), "{mode} benchmark child failed");
    }
}

#[test]
#[ignore = "internal worker for gh496_benchmark_interrupted_resume_against_restart"]
fn gh496_benchmark_worker() {
    let Ok(mode) = dotenvy::var("CASS_GH496_BENCH_MODE") else {
        return;
    };
    let db_path = PathBuf::from(dotenvy::var("CASS_GH496_BENCH_DB").unwrap());
    let data_dir = PathBuf::from(dotenvy::var("CASS_GH496_BENCH_DATA").unwrap());
    let expected_docs = gh496_benchmark_size("CASS_GH496_BENCH_EXPECTED_DOCS", 0);
    let expected_conversations = gh496_benchmark_size("CASS_GH496_BENCH_EXPECTED_CONVERSATIONS", 0);
    let index_path = index_dir(&data_dir).unwrap();
    let reused_docs = if mode == "resume" {
        let scratch = staged_lexical_rebuild_scratch_path(&index_path);
        let candidate =
            lexical_reconcile::canary::exact::PublishedSnapshot::open(&scratch).unwrap();
        let observed = candidate.doc_count() as usize;
        assert_eq!(
            observed,
            gh496_benchmark_size("CASS_GH496_BENCH_REUSED_DOCS", 0)
        );
        observed
    } else {
        0
    };
    let start = Instant::now();
    let outcome =
        rebuild_tantivy_from_db(&db_path, &data_dir, expected_conversations, None).unwrap();
    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(outcome.indexed_docs, expected_docs);
    assert_eq!(outcome.observed_conversations, expected_conversations);
    let snapshot = lexical_reconcile::canary::exact::PublishedSnapshot::open(&index_path).unwrap();
    assert_eq!(snapshot.doc_count() as usize, expected_docs);
    assert!(has_usable_lexical_publication_receipt(
        &index_path,
        &db_path
    ));
    let peak_rss_kib = fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                line.strip_prefix("VmHWM:")?
                    .split_whitespace()
                    .next()?
                    .parse::<u64>()
                    .ok()
            })
        });
    eprintln!(
        "GH496_BENCH {}",
        serde_json::json!({
            "mode": mode,
            "elapsed_ms": elapsed_ms,
            "process_peak_rss_kib": peak_rss_kib,
            "canonical_conversations": expected_conversations,
            "indexed_docs": expected_docs,
            "candidate_docs_before_run": reused_docs,
            "missing_docs": expected_docs - reused_docs,
            "comparison": "same canonical database; separate fresh processes in one driver invocation",
        })
    );
}
