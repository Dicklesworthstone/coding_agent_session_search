// Included by indexer::tests; exercises the production rebuild and real
// filesystem fault seam, not a model of the state machine.

fn gh494_publication_receipt_fixture() -> (TempDir, PathBuf, PathBuf, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();
    let db_path = data_dir.join("db.sqlite");
    let storage = FrankenStorage::open(&db_path).unwrap();
    ensure_fts_schema(&storage);
    seed_lexical_rebuild_fixture(&storage);
    drop(storage);
    rebuild_tantivy_from_db(&db_path, &data_dir, 2, None).unwrap();
    let index_path = index_dir(&data_dir).unwrap();
    assert!(has_usable_lexical_publication_receipt(
        &index_path,
        &db_path
    ));
    (tmp, data_dir, db_path, index_path)
}

#[test]
#[serial_test::serial]
fn gh494_refused_rebuild_preserves_and_bootstraps_prior_publication_authority() {
    #[cfg(windows)]
    const DISK_FULL: i32 = 112;
    #[cfg(not(windows))]
    const DISK_FULL: i32 = libc::ENOSPC;
    let (tmp, data_dir, db_path, index_path) = gh494_publication_receipt_fixture();
    let receipt_path = index_path.join(LEXICAL_PUBLISHED_STATE_FILE);
    let receipt_before = fs::read(&receipt_path).unwrap();
    // Simulate an older binary's completed generation, without the new file.
    fs::rename(&receipt_path, tmp.path().join("saved-receipt.json")).unwrap();
    let manifest_before = fs::read(index_path.join("MANIFEST")).unwrap();
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
    let error = rebuild_tantivy_from_db_with_options(
        &db_path,
        &data_dir,
        2,
        None,
        LexicalRebuildStartupOptions {
            defer_initial_content_fingerprint: true,
        },
        None,
    )
    .expect_err("refuse the actual swap");
    drop(fault);
    assert!(
        error.to_string().contains("publish_staged_generation"),
        "{error:#}"
    );
    assert!(
        !load_lexical_rebuild_state(&index_path)
            .unwrap()
            .unwrap()
            .completed
    );
    assert_eq!(
        fs::read(index_path.join("MANIFEST")).unwrap(),
        manifest_before
    );
    assert_eq!(fs::read(&receipt_path).unwrap(), receipt_before);
    assert!(has_usable_lexical_publication_receipt(
        &index_path,
        &db_path
    ));
    let scratch = staged_lexical_rebuild_scratch_path(&index_path);
    assert!(has_usable_lexical_publication_receipt(&scratch, &db_path));
    rebuild_tantivy_from_db(&db_path, &data_dir, 2, None).unwrap();
    assert!(
        load_lexical_rebuild_state(&index_path)
            .unwrap()
            .unwrap()
            .completed
    );
    assert!(has_usable_lexical_publication_receipt(
        &index_path,
        &db_path
    ));
}

#[test]
#[serial_test::serial]
fn gh494_publication_receipt_rejects_inconsistent_counts_contract_and_identity() {
    let (tmp, _data_dir, db_path, index_path) = gh494_publication_receipt_fixture();
    let receipt_path = index_path.join(LEXICAL_PUBLISHED_STATE_FILE);
    let before = fs::read(&receipt_path).unwrap();
    let good: serde_json::Value = serde_json::from_slice(&before).unwrap();
    for (field, value) in [
        ("version", serde_json::json!(255)),
        ("completed", serde_json::json!(false)),
        ("schema_hash", serde_json::json!("wrong-schema")),
        ("page_size", serde_json::json!(0)),
        ("processed_conversations", serde_json::json!(1)),
        ("committed_offset", serde_json::json!(-1)),
        ("indexed_docs", serde_json::json!(3)),
        (
            "committed_meta_fingerprint",
            serde_json::json!("different-generation"),
        ),
        (
            "pending",
            serde_json::json!({
                "next_offset": 2, "next_conversation_id": 2,
                "processed_conversations": 2, "indexed_docs": 4,
                "base_meta_fingerprint": null,
            }),
        ),
    ] {
        let mut changed = good.clone();
        changed[field] = value;
        write_json_pretty_atomically(&receipt_path, &changed).unwrap();
        assert!(
            !has_usable_lexical_publication_receipt(&index_path, &db_path),
            "{field}"
        );
    }
    for (field, value) in [
        ("db_path", serde_json::json!(tmp.path().join("foreign.db"))),
        ("total_conversations", serde_json::json!(3)),
        ("total_messages", serde_json::json!(0)),
        (
            "storage_fingerprint",
            serde_json::json!("content-pending-v1:2"),
        ),
    ] {
        let mut changed = good.clone();
        changed["db"][field] = value;
        write_json_pretty_atomically(&receipt_path, &changed).unwrap();
        assert!(
            !has_usable_lexical_publication_receipt(&index_path, &db_path),
            "{field}"
        );
    }
    fs::write(&receipt_path, before).unwrap();
    assert!(has_usable_lexical_publication_receipt(
        &index_path,
        &db_path
    ));
    assert!(!has_usable_lexical_publication_receipt(
        &index_path,
        &tmp.path().join("other.db")
    ));
}

#[test]
#[serial_test::serial]
fn gh494_publication_receipt_is_bounded_and_never_repairs_itself_on_read() {
    let (tmp, _data_dir, db_path, index_path) = gh494_publication_receipt_fixture();
    let path = index_path.join(LEXICAL_PUBLISHED_STATE_FILE);
    let checkpoint = fs::read(lexical_rebuild_state_path(&index_path)).unwrap();
    let manifest = fs::read(index_path.join("MANIFEST")).unwrap();
    fs::rename(&path, tmp.path().join("saved-receipt.json")).unwrap();
    assert!(!has_usable_lexical_publication_receipt(
        &index_path,
        &db_path
    ));
    assert!(!path.exists());
    fs::write(&path, b"{").unwrap();
    assert!(!has_usable_lexical_publication_receipt(
        &index_path,
        &db_path
    ));
    assert_eq!(fs::read(&path).unwrap(), b"{");
    let file = OpenOptions::new().write(true).open(&path).unwrap();
    file.set_len(LEXICAL_PUBLISHED_STATE_MAX_BYTES + 1).unwrap();
    drop(file);
    assert!(!has_usable_lexical_publication_receipt(
        &index_path,
        &db_path
    ));
    assert_eq!(
        fs::metadata(&path).unwrap().len(),
        LEXICAL_PUBLISHED_STATE_MAX_BYTES + 1
    );
    assert_eq!(
        fs::read(lexical_rebuild_state_path(&index_path)).unwrap(),
        checkpoint
    );
    assert_eq!(fs::read(index_path.join("MANIFEST")).unwrap(), manifest);
}

fn gh494_query_count(index_path: &Path, term: &str) -> usize {
    use crate::search::query::{FieldMask, SearchClient, SearchFilters};
    SearchClient::open(index_path, None)
        .unwrap()
        .unwrap()
        .search(term, SearchFilters::default(), 100, 0, FieldMask::FULL)
        .unwrap()
        .len()
}

fn gh494_missing_staged_generation_replays_canonical_content(pending: bool, legacy: bool) {
    #[cfg(windows)]
    const DISK_FULL: i32 = 112;
    #[cfg(not(windows))]
    const DISK_FULL: i32 = libc::ENOSPC;

    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();
    let db_path = data_dir.join("db.sqlite");
    let storage = FrankenStorage::open(&db_path).unwrap();
    ensure_fts_schema(&storage);
    seed_lexical_rebuild_fixture(&storage);
    drop(storage);
    let index_path = index_dir(&data_dir).unwrap();
    let old_messages = (0..4)
        .map(|idx| {
            let mut message = norm_msg(idx, 1_700_000_000_000 + idx);
            message.content = format!("priorgenerationneedle {idx}");
            message
        })
        .collect();
    let old_conversation = norm_conv(Some("prior-generation"), old_messages);
    let mut old = TantivyIndex::open_or_create(&index_path).unwrap();
    old.add_messages_with_conversation_id(&old_conversation, &old_conversation.messages, Some(900))
        .unwrap();
    old.commit().unwrap();
    drop(old);
    assert_eq!(gh494_query_count(&index_path, "priorgenerationneedle"), 4);
    assert_eq!(gh494_query_count(&index_path, "fixture"), 0);
    let old_manifest = fs::read(index_path.join("MANIFEST")).unwrap();

    // Produce a genuine complete staged build that is refused at the swap.
    // The old generation has EXACTLY the same count but different content:
    // document-count validation alone cannot prove which cursor it owns.
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
    let error = rebuild_tantivy_from_db(&db_path, &data_dir, 2, None)
        .expect_err("the first publication must reach the injected refusal");
    drop(fault);
    assert!(
        error.to_string().contains("publish_staged_generation"),
        "{error:#}"
    );
    let scratch = staged_lexical_rebuild_scratch_path(&index_path);
    assert_eq!(gh494_query_count(&scratch, "fixture"), 4);
    assert_eq!(fs::read(index_path.join("MANIFEST")).unwrap(), old_manifest);
    let mut interrupted = load_lexical_rebuild_state(&index_path).unwrap().unwrap();
    assert_eq!(
        interrupted.execution_mode,
        Some(LexicalRebuildExecutionMode::StagedSingleIndex)
    );
    assert!(!interrupted.completed);
    if legacy {
        interrupted.set_execution_mode(LexicalRebuildExecutionMode::SharedWriter);
        persist_lexical_rebuild_state(&index_path, &interrupted).unwrap();
    }
    if pending {
        // Also cover death between the content commit and cursor finalization.
        // With scratch missing, comparing this base with old live would falsely
        // promote the pending EOF cursor without ever reading canonical rows.
        interrupted.record_pending_commit(interrupted.committed_conversation_id, 2, 4, None);
        interrupted.committed_offset = 0;
        interrupted.committed_conversation_id = None;
        interrupted.processed_conversations = 0;
        interrupted.indexed_docs = 0;
        persist_lexical_rebuild_state(&index_path, &interrupted).unwrap();
    }
    let missing_candidate = tmp.path().join("removed-from-staging-but-retained");
    fs::rename(&scratch, &missing_candidate).unwrap();
    let missing_manifest = fs::read(missing_candidate.join("MANIFEST")).unwrap();

    let rebuilt = rebuild_tantivy_from_db(&db_path, &data_dir, 2, None).unwrap();
    assert!(rebuilt.exact_checkpoint_persisted);
    assert_eq!(rebuilt.indexed_docs, 4);
    assert_eq!(rebuilt.observed_conversations, 2);
    assert_eq!(gh494_query_count(&index_path, "fixture"), 4);
    assert_eq!(gh494_query_count(&index_path, "priorgenerationneedle"), 0);
    let completed = load_lexical_rebuild_state(&index_path).unwrap().unwrap();
    assert!(completed.completed);
    assert_eq!(
        completed.execution_mode,
        Some(LexicalRebuildExecutionMode::SharedWriter)
    );
    let quarantines: Vec<_> = fs::read_dir(index_path.parent().unwrap())
        .unwrap()
        .map(Result::unwrap)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".lexical-rebuild-quarantine-")
        })
        .collect();
    assert_eq!(quarantines.len(), 1);
    assert_eq!(
        load_lexical_rebuild_state(&quarantines[0].path())
            .unwrap()
            .unwrap(),
        interrupted,
        "preserve the exact lost candidate cursor, including pending work"
    );
    assert_eq!(
        fs::read(missing_candidate.join("MANIFEST")).unwrap(),
        missing_manifest
    );
    let published = fs::read(index_path.join("MANIFEST")).unwrap();
    assert!(
        !rebuild_tantivy_from_db(&db_path, &data_dir, 2, None)
            .unwrap()
            .exact_checkpoint_persisted
    );
    assert_eq!(fs::read(index_path.join("MANIFEST")).unwrap(), published);
}

#[test]
#[serial_test::serial]
fn gh494_missing_staged_eof_cannot_certify_same_count_prior_live_content() {
    gh494_missing_staged_generation_replays_canonical_content(false, false);
}

#[test]
#[serial_test::serial]
fn gh494_missing_staged_pending_commit_cannot_be_promoted_by_prior_live() {
    gh494_missing_staged_generation_replays_canonical_content(true, false);
}

#[test]
#[serial_test::serial]
fn gh494_legacy_cursor_without_its_generation_replays_instead_of_guessing() {
    gh494_missing_staged_generation_replays_canonical_content(false, true);
}

#[test]
#[serial_test::serial]
fn gh494_explicit_live_resume_ignores_unrelated_scratch_generation() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();
    let db_path = data_dir.join("db.sqlite");
    let storage = FrankenStorage::open(&db_path).unwrap();
    ensure_fts_schema(&storage);
    seed_lexical_rebuild_fixture(&storage);
    drop(storage);
    rebuild_tantivy_from_db(&db_path, &data_dir, 2, None).unwrap();
    let index_path = index_dir(&data_dir).unwrap();
    let scratch = staged_lexical_rebuild_scratch_path(&index_path);
    let mut unrelated = TantivyIndex::open_or_create(&scratch).unwrap();
    unrelated.commit().unwrap();
    drop(unrelated);
    let unrelated_manifest = fs::read(scratch.join("MANIFEST")).unwrap();
    let mut state = load_lexical_rebuild_state(&index_path).unwrap().unwrap();
    state.completed = false;
    state.set_execution_mode(LexicalRebuildExecutionMode::LiveSingleIndex);
    let live_fingerprint = index_meta_fingerprint(&index_path).unwrap();
    state.record_pending_commit(Some(999), 999, 999, live_fingerprint);
    persist_lexical_rebuild_state(&index_path, &state).unwrap();
    let reconciled = reconcile_pending_lexical_commit(&index_path, state).unwrap();
    assert_eq!(reconciled.processed_conversations, 2);
    assert_eq!(reconciled.indexed_docs, 4);
    assert!(reconciled.pending.is_none());
    assert_eq!(
        lexical_rebuild_resume_content_path(&index_path, &reconciled).unwrap(),
        index_path
    );
    let resumed = rebuild_tantivy_from_db(&db_path, &data_dir, 2, None).unwrap();
    assert_eq!(resumed.indexed_docs, 4);
    assert_eq!(gh494_query_count(&index_path, "fixture"), 4);
    assert_eq!(
        fs::read(scratch.join("MANIFEST")).unwrap(),
        unrelated_manifest
    );
}

#[test]
fn gh494_bound_staged_reconciliation_refuses_missing_content_without_mutation() {
    let tmp = TempDir::new().unwrap();
    let index_path = tmp.path().join("index");
    fs::create_dir(&index_path).unwrap();
    let mut state = LexicalRebuildState::new(
        LexicalRebuildDbState {
            db_path: tmp.path().join("db.sqlite").to_string_lossy().into_owned(),
            total_conversations: 2,
            total_messages: 4,
            storage_fingerprint: "content-v1:2:2:4".to_owned(),
        },
        LEXICAL_REBUILD_PAGE_SIZE,
    );
    state.set_execution_mode(LexicalRebuildExecutionMode::StagedSingleIndex);
    state.record_pending_commit(Some(2), 2, 4, None);
    persist_lexical_rebuild_state(&index_path, &state).unwrap();
    let before = fs::read(lexical_rebuild_state_path(&index_path)).unwrap();
    let error = reconcile_pending_lexical_commit(&index_path, state.clone()).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("staged lexical generation is unavailable")
    );
    assert_eq!(
        fs::read(lexical_rebuild_state_path(&index_path)).unwrap(),
        before
    );
    let scratch = staged_lexical_rebuild_scratch_path(&index_path);
    fs::write(&scratch, "not a directory").unwrap();
    assert!(!lexical_rebuild_bound_candidate_is_missing(&index_path, &state).unwrap());
    assert!(lexical_rebuild_resume_content_path(&index_path, &state).is_err());
    assert_eq!(fs::read_to_string(scratch).unwrap(), "not a directory");
    assert_eq!(
        fs::read(lexical_rebuild_state_path(&index_path)).unwrap(),
        before
    );
}

fn gh494_restart_damaged_candidate(damage: &str) {
    #[cfg(windows)]
    const DISK_FULL: i32 = 112;
    #[cfg(not(windows))]
    const DISK_FULL: i32 = libc::ENOSPC;

    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();
    let db_path = data_dir.join("db.sqlite");
    let storage = FrankenStorage::open(&db_path).unwrap();
    ensure_fts_schema(&storage);
    seed_lexical_rebuild_fixture(&storage);
    drop(storage);
    rebuild_tantivy_from_db(&db_path, &data_dir, 2, None).unwrap();
    let index_path = index_dir(&data_dir).unwrap();
    let old_live_manifest = fs::read(index_path.join("MANIFEST")).unwrap();
    let mut checkpoint = load_lexical_rebuild_state(&index_path).unwrap().unwrap();
    assert_eq!(checkpoint.indexed_docs, 4);
    assert_eq!(checkpoint.processed_conversations, 2);
    let scratch = staged_lexical_rebuild_scratch_path(&index_path);
    fs::create_dir(&scratch).unwrap();
    if damage == "short-prefix" {
        let mut empty = TantivyIndex::open_or_create(&scratch).unwrap();
        empty.commit().unwrap();
        drop(empty);
    } else {
        for entry in walkdir::WalkDir::new(&index_path).min_depth(1) {
            let entry = entry.unwrap();
            let target = scratch.join(entry.path().strip_prefix(&index_path).unwrap());
            if entry.file_type().is_dir() {
                fs::create_dir_all(target).unwrap();
            } else {
                assert!(entry.file_type().is_file());
                fs::copy(entry.path(), target).unwrap();
            }
        }
    }
    match damage {
        "corrupt-manifest" => {
            fs::write(scratch.join("MANIFEST"), b"invalid quill manifest").unwrap();
        }
        "missing-manifest" => {
            fs::rename(
                scratch.join("MANIFEST"),
                scratch.join("MANIFEST.before-loss"),
            )
            .unwrap();
        }
        "schema-mismatch" => {
            fs::write(
                scratch.join("schema_hash.json"),
                b"old incompatible contract",
            )
            .unwrap();
        }
        "short-prefix" => {}
        other => panic!("unknown damage fixture: {other}"),
    }
    fs::write(scratch.join("failure-evidence"), damage).unwrap();
    let evidence_before: BTreeMap<_, _> = walkdir::WalkDir::new(&scratch)
        .into_iter()
        .map(Result::unwrap)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| {
            (
                entry.path().strip_prefix(&scratch).unwrap().to_path_buf(),
                fs::read(entry.path()).unwrap(),
            )
        })
        .collect();
    checkpoint.completed = false;
    checkpoint.pending = None;
    persist_lexical_rebuild_state(&index_path, &checkpoint).unwrap();

    // Stop at the real swap after recovery, so we can inspect BOTH the preserved
    // prior live publication and the newly rebuilt, certified candidate.
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
    let error = rebuild_tantivy_from_db(&db_path, &data_dir, 2, None)
        .expect_err("real swap fault must be reached after a complete replay");
    drop(fault);
    assert!(
        error.to_string().contains("publish_staged_generation"),
        "{error:#}"
    );
    assert_eq!(
        error
            .downcast_ref::<std::io::Error>()
            .unwrap()
            .raw_os_error(),
        Some(DISK_FULL)
    );
    assert_eq!(
        fs::read(index_path.join("MANIFEST")).unwrap(),
        old_live_manifest
    );
    let candidate = load_lexical_rebuild_state(&scratch).unwrap().unwrap();
    assert!(candidate.completed);
    assert_eq!(
        candidate.processed_conversations, 2,
        "old counters must not survive reset"
    );
    assert_eq!(
        candidate.indexed_docs, 4,
        "replayed docs must not be double-counted"
    );
    verify_published_lexical_doc_count(&scratch, 4, "gh494 restarted candidate").unwrap();

    let quarantines: Vec<_> = fs::read_dir(index_path.parent().unwrap())
        .unwrap()
        .map(Result::unwrap)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".lexical-rebuild-quarantine-")
        })
        .collect();
    assert_eq!(
        quarantines.len(),
        1,
        "exactly the failed candidate must be retained"
    );
    let quarantine = quarantines[0].path();
    for (path, before) in evidence_before {
        assert_eq!(
            fs::read(quarantine.join("index").join(path)).unwrap(),
            before
        );
    }
    assert_eq!(
        load_lexical_rebuild_state(&quarantine).unwrap().unwrap(),
        checkpoint
    );
    // Even an old quarantine must never be classified as disposable staging.
    staging_reclaim::reclaim_orphaned_staging_dirs_for_data_dir(
        &data_dir,
        SystemTime::now() + Duration::from_secs(86_400),
    );
    assert!(quarantine.join("index/failure-evidence").is_file());

    let resumed = rebuild_tantivy_from_db(&db_path, &data_dir, 2, None).unwrap();
    assert_eq!(resumed.observed_conversations, 2);
    assert_eq!(resumed.observed_messages, Some(4));
    assert_eq!(resumed.indexed_docs, 4);
    let completed = load_lexical_rebuild_state(&index_path).unwrap().unwrap();
    assert!(completed.completed);
    assert_eq!(completed.processed_conversations, 2);
    verify_published_lexical_doc_count(&index_path, 4, "gh494 recovered publication").unwrap();
    let generation = fs::read(index_path.join("lexical-generation-manifest.json")).unwrap();
    assert!(
        !rebuild_tantivy_from_db(&db_path, &data_dir, 2, None)
            .unwrap()
            .exact_checkpoint_persisted
    );
    assert_eq!(
        fs::read(index_path.join("lexical-generation-manifest.json")).unwrap(),
        generation
    );
}

#[test]
#[serial_test::serial]
fn gh494_corrupt_candidate_restarts_with_zero_counters_and_retains_evidence() {
    gh494_restart_damaged_candidate("corrupt-manifest");
}

#[test]
#[serial_test::serial]
fn gh494_missing_candidate_manifest_replays_the_committed_prefix() {
    gh494_restart_damaged_candidate("missing-manifest");
}

#[test]
#[serial_test::serial]
fn gh494_candidate_schema_mismatch_is_quarantined_before_writer_open() {
    gh494_restart_damaged_candidate("schema-mismatch");
}

#[test]
#[serial_test::serial]
fn gh494_readable_but_short_candidate_cannot_reuse_the_eof_cursor() {
    gh494_restart_damaged_candidate("short-prefix");
}

#[test]
#[serial_test::serial]
fn gh494_publish_failure_at_eof_is_finalized_on_retry_instead_of_returning_success() {
    #[cfg(windows)]
    const ENOSPC_RAW_OS_ERROR: i32 = 112; // ERROR_DISK_FULL
    #[cfg(not(windows))]
    const ENOSPC_RAW_OS_ERROR: i32 = libc::ENOSPC;

    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();
    let db_path = data_dir.join("db.sqlite");
    let storage = FrankenStorage::open(&db_path).unwrap();
    ensure_fts_schema(&storage);
    seed_lexical_rebuild_fixture(&storage);
    let index_path = index_dir(&data_dir).unwrap();
    let mut previous = TantivyIndex::open_or_create(&index_path).unwrap();
    previous.commit().unwrap();
    drop(previous);
    verify_published_lexical_doc_count(&index_path, 0, "gh494 prior live").unwrap();

    // Fail AFTER every row was committed/folded, at the actual swap.
    // Linux rolls the exchange back; the rename-pair seam restores
    // the prior live tree. Both retain the complete new candidate.
    #[cfg(target_os = "linux")]
    let fault = inject_lexical_publish_rename_failure_once(
        LexicalPublishRenameSite::LinuxParkPriorLiveToCanonicalSidecar,
        ENOSPC_RAW_OS_ERROR,
    );
    #[cfg(not(target_os = "linux"))]
    let fault = inject_lexical_publish_rename_failure_once(
        LexicalPublishRenameSite::NonLinuxPublishStagedLive,
        ENOSPC_RAW_OS_ERROR,
    );
    let error = rebuild_tantivy_from_db(&db_path, &data_dir, 2, None)
        .expect_err("refused publication must fail, not report completion");
    drop(fault);
    let message = error.to_string();
    assert!(message.contains("publish_staged_generation"), "{message}");
    assert!(message.contains("indexed_docs=4"), "{message}");
    assert!(message.contains("processed_conversations=2"), "{message}");
    assert_eq!(
        error
            .downcast_ref::<std::io::Error>()
            .unwrap()
            .raw_os_error(),
        Some(ENOSPC_RAW_OS_ERROR)
    );

    let interrupted = load_lexical_rebuild_state(&index_path).unwrap().unwrap();
    assert!(!interrupted.completed);
    assert_eq!(interrupted.reported_processed_conversations(), 2);
    assert_eq!(interrupted.reported_indexed_docs(), 4);
    verify_published_lexical_doc_count(&index_path, 0, "gh494 preserved prior live").unwrap();
    let scratch = staged_lexical_rebuild_scratch_path(&index_path);
    verify_published_lexical_doc_count(&scratch, 4, "gh494 retained candidate").unwrap();
    let certified = load_lexical_rebuild_state(&scratch).unwrap().unwrap();
    assert!(
        certified.completed,
        "swap must receive a certified candidate"
    );
    assert!(certified.pending.is_none());
    assert!(scratch.join("lexical-generation-manifest.json").is_file());

    // A real refused swap must also leave a diagnostic after the process exits,
    // without certifying the checkpoint or replacing the original OS error.
    let report: serde_json::Value = serde_json::from_slice(
        &fs::read(index_path.join(".lexical-rebuild-last-failure.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(report["phase"], "publish_staged_generation");
    assert_eq!(report["expected_docs"], 4);
    assert_eq!(report["processed_conversations"], 2);
    assert_eq!(report["build_path"], serde_json::json!(scratch));
    assert_eq!(report["live_path"], serde_json::json!(index_path));
    assert!(
        report["error_chain"]
            .as_str()
            .unwrap()
            .contains(&std::io::Error::from_raw_os_error(ENOSPC_RAW_OS_ERROR).to_string())
    );

    // Refuse certification through a real filesystem obstruction. Neither
    // repeated refusal may replace the old live generation. Before GH494's
    // atomic certification change, this failure happened AFTER the swap.
    let old_live_manifest = fs::read(index_path.join("MANIFEST")).unwrap();
    let candidate_manifest = scratch.join("lexical-generation-manifest.json");
    fs::rename(
        &candidate_manifest,
        scratch.join("generation-before-certification-refusal.json"),
    )
    .unwrap();
    fs::create_dir(&candidate_manifest).unwrap();
    for _ in 0..2 {
        let error = rebuild_tantivy_from_db(&db_path, &data_dir, 2, None)
            .expect_err("candidate certification refusal must fail before swapping");
        assert!(
            error.to_string().contains("persist_generation_manifest"),
            "{error:#}"
        );
        assert!(error.downcast_ref::<std::io::Error>().is_some());
        assert_eq!(
            fs::read(index_path.join("MANIFEST")).unwrap(),
            old_live_manifest
        );
        assert!(
            !load_lexical_rebuild_state(&index_path)
                .unwrap()
                .unwrap()
                .completed
        );
        verify_published_lexical_doc_count(&scratch, 4, "gh494 refused candidate").unwrap();
    }
    fs::rename(
        &candidate_manifest,
        scratch.join("preserved-certification-obstruction"),
    )
    .unwrap();

    // Before the fix, reconciliation advanced the cursor to EOF and
    // returned Ok here without swapping or completing the checkpoint.
    let resumed = rebuild_tantivy_from_db(&db_path, &data_dir, 2, None).unwrap();
    assert!(
        resumed.exact_checkpoint_persisted,
        "EOF still owes publication"
    );
    assert_eq!(resumed.indexed_docs, 4);
    assert_eq!(resumed.observed_messages, Some(4));
    let completed = load_lexical_rebuild_state(&index_path).unwrap().unwrap();
    assert!(completed.completed);
    assert!(completed.pending.is_none());
    verify_published_lexical_doc_count(&index_path, 4, "gh494 published retry").unwrap();
    assert!(!scratch.exists(), "finished candidate must leave staging");
    let manifest_path = index_path.join("lexical-generation-manifest.json");
    let manifest = fs::read(&manifest_path).unwrap();
    assert!(!manifest.is_empty());

    // Repeated invocations reuse the completed publication rather
    // than manufacturing another generation from the EOF cursor.
    let reused = rebuild_tantivy_from_db(&db_path, &data_dir, 2, None).unwrap();
    assert!(!reused.exact_checkpoint_persisted);
    assert_eq!(reused.indexed_docs, 4);
    assert_eq!(fs::read(&manifest_path).unwrap(), manifest);
    assert_eq!(
        load_lexical_rebuild_state(&index_path).unwrap().unwrap(),
        completed
    );
    verify_published_lexical_doc_count(&index_path, 4, "gh494 reused publication").unwrap();
}

#[test]
#[serial_test::serial]
fn gh494_zero_conversations_still_publish_a_readable_completed_generation() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();
    let db_path = data_dir.join("db.sqlite");
    let storage = FrankenStorage::open(&db_path).unwrap();
    ensure_fts_schema(&storage);
    let rebuilt = rebuild_tantivy_from_db(&db_path, &data_dir, 0, None).unwrap();
    assert!(rebuilt.exact_checkpoint_persisted);
    assert_eq!(rebuilt.indexed_docs, 0);
    let index_path = index_dir(&data_dir).unwrap();
    assert!(
        load_lexical_rebuild_state(&index_path)
            .unwrap()
            .unwrap()
            .completed
    );
    verify_published_lexical_doc_count(&index_path, 0, "gh494 empty publication").unwrap();
    assert!(
        index_path
            .join("lexical-generation-manifest.json")
            .is_file()
    );
}

#[test]
#[serial_test::serial]
fn gh494_completed_marker_cannot_hide_a_live_document_count_mismatch() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();
    let db_path = data_dir.join("db.sqlite");
    let storage = FrankenStorage::open(&db_path).unwrap();
    ensure_fts_schema(&storage);
    seed_lexical_rebuild_fixture(&storage);
    rebuild_tantivy_from_db(&db_path, &data_dir, 2, None).unwrap();
    let index_path = index_dir(&data_dir).unwrap();
    let mut checkpoint = load_lexical_rebuild_state(&index_path).unwrap().unwrap();
    checkpoint.indexed_docs += 1;
    persist_lexical_rebuild_state(&index_path, &checkpoint).unwrap();
    let error = rebuild_tantivy_from_db(&db_path, &data_dir, 2, None)
        .expect_err("completed marker cannot certify a mismatched publication");
    assert!(
        error.to_string().contains("reuse_completed_generation"),
        "{error:#}"
    );
    verify_published_lexical_doc_count(&index_path, 4, "gh494 mismatch kept live").unwrap();
}

#[test]
#[serial_test::serial]
#[cfg(target_os = "linux")]
fn gh494_post_swap_io_failure_preserves_certification_and_reuses_the_generation() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();
    let db_path = data_dir.join("db.sqlite");
    let storage = FrankenStorage::open(&db_path).unwrap();
    ensure_fts_schema(&storage);
    seed_lexical_rebuild_fixture(&storage);
    let index_path = index_dir(&data_dir).unwrap();
    let mut previous = TantivyIndex::open_or_create(&index_path).unwrap();
    previous.commit().unwrap();
    drop(previous);

    // The existing crash-window seam writes its sentinel AFTER the real
    // Linux swap and sidecar park. A regular file as its parent makes that
    // write fail through actual filesystem I/O, without adding a mock seam.
    let blocked_parent = tmp.path().join("sentinel-parent-is-a-file");
    fs::write(&blocked_parent, b"preserve").unwrap();
    let sentinel = blocked_parent.join("sentinel.json");
    let guard = set_env_var(
        "CASS_TEST_LEXICAL_PUBLISH_KILL_RELAUNCH_SENTINEL",
        sentinel.to_str().unwrap(),
    );
    let error = rebuild_tantivy_from_db(&db_path, &data_dir, 2, None)
        .expect_err("post-swap I/O failure must remain visible");
    drop(guard);
    assert!(
        error.to_string().contains("publish_staged_generation"),
        "{error:#}"
    );
    assert!(error.downcast_ref::<std::io::Error>().is_some());
    verify_published_lexical_doc_count(&index_path, 4, "gh494 swapped generation").unwrap();
    let completed = load_lexical_rebuild_state(&index_path).unwrap().unwrap();
    assert!(
        completed.completed,
        "completed receipt must move with the candidate"
    );
    assert!(completed.pending.is_none());
    let generation_path = index_path.join("lexical-generation-manifest.json");
    let generation = fs::read(&generation_path).unwrap();
    let engine_manifest = fs::read(index_path.join("MANIFEST")).unwrap();
    let sidecar = lexical_publish_in_progress_backup_path(&index_path);
    assert!(
        sidecar.is_dir(),
        "failure must be after the old generation was parked"
    );
    verify_published_lexical_doc_count(&sidecar, 0, "gh494 parked prior live").unwrap();

    let reused = rebuild_tantivy_from_db(&db_path, &data_dir, 2, None).unwrap();
    assert!(
        !reused.exact_checkpoint_persisted,
        "retry must not rebuild or recertify"
    );
    assert_eq!(reused.indexed_docs, 4);
    assert_eq!(fs::read(&generation_path).unwrap(), generation);
    assert_eq!(
        fs::read(index_path.join("MANIFEST")).unwrap(),
        engine_manifest
    );
    assert_eq!(
        load_lexical_rebuild_state(&index_path).unwrap().unwrap(),
        completed
    );
    assert!(
        !sidecar.exists(),
        "completed reuse must finish backup retention"
    );
    assert_eq!(fs::read(blocked_parent).unwrap(), b"preserve");
}

#[test]
fn gh494_pending_staged_commit_does_not_advance_when_only_live_differs() {
    let tmp = TempDir::new().unwrap();
    let live = tmp.path().join("index");
    let scratch = staged_lexical_rebuild_scratch_path(&live);
    fs::create_dir_all(&live).unwrap();
    fs::create_dir_all(&scratch).unwrap();
    let marker = crate::search::quill_bridge::QUILL_INDEX_MARKER;
    fs::write(live.join(marker), b"unrelated prior live manifest").unwrap();
    fs::write(scratch.join(marker), b"candidate before pending commit").unwrap();
    let mut state = LexicalRebuildState::new(
        LexicalRebuildDbState {
            db_path: "/fixture/agent_search.db".to_string(),
            total_conversations: 2,
            total_messages: 4,
            storage_fingerprint: "content-v1:2:2:4".to_string(),
        },
        LEXICAL_REBUILD_PAGE_SIZE,
    );
    state.record_pending_commit(Some(2), 2, 4, index_meta_fingerprint(&scratch).unwrap());
    let reconciled = reconcile_pending_lexical_commit(&live, state).unwrap();
    assert_eq!(
        reconciled.processed_conversations, 0,
        "uncommitted rows must replay"
    );
    assert_eq!(reconciled.indexed_docs, 0);
    assert!(reconciled.pending.is_none());
    assert_eq!(
        load_lexical_rebuild_state(&live).unwrap().unwrap(),
        reconciled
    );
}

#[test]
fn gh494_pending_staged_commit_advances_from_the_candidate_manifest() {
    let tmp = TempDir::new().unwrap();
    let live = tmp.path().join("index");
    let scratch = staged_lexical_rebuild_scratch_path(&live);
    fs::create_dir_all(&live).unwrap();
    fs::create_dir_all(&scratch).unwrap();
    let marker = crate::search::quill_bridge::QUILL_INDEX_MARKER;
    fs::write(live.join(marker), b"same initial manifest").unwrap();
    fs::write(scratch.join(marker), b"same initial manifest").unwrap();
    let mut state = LexicalRebuildState::new(
        LexicalRebuildDbState {
            db_path: "/fixture/agent_search.db".to_string(),
            total_conversations: 2,
            total_messages: 4,
            storage_fingerprint: "content-v1:2:2:4".to_string(),
        },
        LEXICAL_REBUILD_PAGE_SIZE,
    );
    state.record_pending_commit(Some(2), 2, 4, index_meta_fingerprint(&scratch).unwrap());
    fs::write(scratch.join(marker), b"candidate after durable commit").unwrap();
    let reconciled = reconcile_pending_lexical_commit(&live, state).unwrap();
    assert_eq!(reconciled.processed_conversations, 2);
    assert_eq!(reconciled.indexed_docs, 4);
    assert_eq!(reconciled.committed_conversation_id, Some(2));
    assert_eq!(
        reconciled.committed_meta_fingerprint,
        index_meta_fingerprint(&scratch).unwrap()
    );
    assert!(reconciled.pending.is_none());
    assert_eq!(
        load_lexical_rebuild_state(&live).unwrap().unwrap(),
        reconciled
    );
}
