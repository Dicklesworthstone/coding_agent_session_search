// Included by indexer::tests; exercises the production rebuild and real
// filesystem fault seam, not a model of the state machine.

#[test]
#[serial_test::serial]
fn gh494_publish_failure_at_eof_is_finalized_on_retry_instead_of_returning_success() {
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
