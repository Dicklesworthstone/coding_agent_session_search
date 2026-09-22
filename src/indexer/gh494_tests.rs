// Included by indexer::tests; exercises the production rebuild and real
// filesystem fault seam, not a model of the state machine.

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
        .err()
        .expect("refused publication must fail, not report completion");
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
            .err()
            .expect("candidate certification refusal must fail before swapping");
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
        .err()
        .expect("completed marker cannot certify a mismatched publication");
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
        .err()
        .expect("post-swap I/O failure must remain visible");
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
