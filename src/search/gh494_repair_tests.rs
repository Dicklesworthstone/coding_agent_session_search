// Included in search_lexical_self_heal_tests to exercise the production
// admission and publication checks with its real canonical-database fixture.

fn gh494_completed_search_repair_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, usize) {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = seed_canonical_search_db(tmp.path());
    let outcome = crate::indexer::repair_lexical_index_from_canonical_db_for_search(
        &db_path,
        tmp.path(),
        None,
    )
    .unwrap();
    let index_path = crate::search::tantivy::expected_index_dir(tmp.path());
    (tmp, db_path, index_path, outcome.indexed_docs)
}

fn gh494_edit_checkpoint(index_path: &Path, edit: impl FnOnce(&mut serde_json::Value)) {
    let path = index_path.join(".lexical-rebuild-state.json");
    let mut checkpoint: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    edit(&mut checkpoint);
    std::fs::write(path, serde_json::to_vec_pretty(&checkpoint).unwrap()).unwrap();
}

#[test]
#[serial_test::serial]
fn gh494_search_repair_success_requires_a_completed_readable_publication() {
    let (_tmp, db_path, index_path, indexed_docs) = gh494_completed_search_repair_fixture();
    assert!(search_existing_lexical_generation_is_usable(&index_path, &db_path).unwrap());
    verify_search_lexical_repair_publication(&index_path, &db_path, indexed_docs).unwrap();
    gh494_edit_checkpoint(&index_path, |checkpoint| {
        checkpoint["completed"] = serde_json::json!(false);
    });
    // The old immutable generation may still answer queries, but that must
    // never turn the unfinished REPAIR into a successful publication.
    assert!(search_existing_lexical_generation_is_usable(&index_path, &db_path).unwrap());
    let error =
        verify_search_lexical_repair_publication(&index_path, &db_path, indexed_docs).unwrap_err();
    assert!(error.to_string().contains("publication is incomplete"));
}

#[test]
#[serial_test::serial]
fn gh494_incomplete_rebuild_without_independent_receipt_is_not_searchable() {
    let (tmp, db_path, index_path, _) = gh494_completed_search_repair_fixture();
    std::fs::rename(
        index_path.join(".lexical-published-state.json"),
        tmp.path().join("saved-publication-receipt.json"),
    )
    .unwrap();
    gh494_edit_checkpoint(&index_path, |checkpoint| {
        checkpoint["completed"] = serde_json::json!(false);
    });
    assert!(!search_existing_lexical_generation_is_usable(&index_path, &db_path).unwrap());
}

#[test]
#[serial_test::serial]
fn gh494_active_rebuild_serves_certified_prior_generation_without_joining() {
    let (tmp, db_path, index_path, _) = gh494_completed_search_repair_fixture();
    gh494_edit_checkpoint(&index_path, |checkpoint| {
        checkpoint["completed"] = serde_json::json!(false);
        checkpoint["processed_conversations"] = serde_json::json!(0);
        checkpoint["indexed_docs"] = serde_json::json!(0);
    });
    let _lock = hold_active_index_run_lock(tmp.path(), &db_path);
    let before = data_tree_snapshot(tmp.path());
    let healed = ensure_lexical_assets_for_search(
        tmp.path(),
        &db_path,
        &index_path,
        Some(0),
        Instant::now(),
        false,
        true,
    )
    .unwrap();
    assert_eq!(healed.action, "active-rebuild-searching-existing-index");
    let (strict, opened) =
        inspect_lexical_assets_for_search_read_only(tmp.path(), &db_path, &index_path).unwrap();
    assert_eq!(
        strict.action,
        "no-maintenance-active-rebuild-searching-existing-index"
    );
    assert!(opened.is_some());
    assert_eq!(data_tree_snapshot(tmp.path()), before);
}

#[test]
#[serial_test::serial]
fn gh494_search_repair_cannot_certify_a_missing_checkpoint() {
    let (tmp, db_path, index_path, indexed_docs) = gh494_completed_search_repair_fixture();
    std::fs::rename(
        index_path.join(".lexical-rebuild-state.json"),
        tmp.path().join("saved-checkpoint.json"),
    )
    .unwrap();
    let error =
        verify_search_lexical_repair_publication(&index_path, &db_path, indexed_docs).unwrap_err();
    assert!(error.to_string().contains("without a published checkpoint"));
    assert!(tmp.path().join("saved-checkpoint.json").is_file());
}

#[test]
#[serial_test::serial]
fn gh494_search_lock_race_cannot_admit_a_foreign_generation() {
    let (tmp, db_path, index_path, indexed_docs) = gh494_completed_search_repair_fixture();
    gh494_edit_checkpoint(&index_path, |checkpoint| {
        checkpoint["db"]["db_path"] = serde_json::json!(tmp.path().join("different.sqlite"));
    });
    assert!(!search_existing_lexical_generation_is_usable(&index_path, &db_path).unwrap());
    let error =
        verify_search_lexical_repair_publication(&index_path, &db_path, indexed_docs).unwrap_err();
    assert!(error.to_string().contains("publication is unusable"));
}

#[test]
#[serial_test::serial]
fn gh494_search_repair_checks_live_documents_not_just_the_completed_marker() {
    let (_tmp, db_path, index_path, indexed_docs) = gh494_completed_search_repair_fixture();
    let impossible_docs = indexed_docs.checked_add(1).unwrap();
    gh494_edit_checkpoint(&index_path, |checkpoint| {
        checkpoint["indexed_docs"] = serde_json::json!(impossible_docs);
    });
    let error = verify_search_lexical_repair_publication(&index_path, &db_path, impossible_docs)
        .unwrap_err();
    assert!(
        error.to_string().contains("search repair completion"),
        "{error:#}"
    );
}
