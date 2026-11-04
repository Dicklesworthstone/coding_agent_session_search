// Included after gh496_tests.rs by indexer::tests. Completion is authority
// over a source generation, not an inference from equal document counts.

#[test]
#[serial_test::serial]
fn gh496_legacy_completed_checkpoint_cannot_certify_same_count_content_edit() {
    let fixture = gh496_fixture();
    rebuild_tantivy_from_db(&fixture.db_path, &fixture.data_dir, 2, None).unwrap();
    gh496_assert_exact(&fixture, 2);
    let mut legacy = load_lexical_rebuild_state(&fixture.index_path)
        .unwrap()
        .unwrap();
    assert!(legacy.completed);
    // Simulate the completed v3 checkpoints already deployed on main. Their
    // count/ID fingerprint records no proof of the old source write epoch.
    legacy.resume_binding = None;
    legacy.completed_source_epoch = None;
    persist_lexical_rebuild_state(&fixture.index_path, &legacy).unwrap();

    let storage = FrankenStorage::open(&fixture.db_path).unwrap();
    let before = lexical_rebuild_db_state(&storage, &fixture.db_path).unwrap();
    storage
        .raw()
        .execute(
            "UPDATE messages SET content = 'growthproof legacycompletionrewrite' \
             WHERE conversation_id = 1 AND idx = 0",
        )
        .unwrap();
    let after = lexical_rebuild_db_state(&storage, &fixture.db_path).unwrap();
    assert_eq!(before, after, "count/ID evidence must miss this edit");
    assert!(!legacy.matches_run(&after, LEXICAL_REBUILD_PAGE_SIZE));
    let _historical_counts_refreshed =
        persist_completed_lexical_rebuild_checkpoint_from_observations(
            &storage,
            &fixture.index_path,
            after.clone(),
            after.total_messages,
            max_conversation_id_exact(&storage).unwrap(),
            live_tantivy_doc_count(&fixture.index_path).unwrap(),
            true,
        )
        .unwrap();
    let refreshed = load_lexical_rebuild_state(&fixture.index_path)
        .unwrap()
        .unwrap();
    assert!(refreshed.resume_binding.is_none());
    assert!(refreshed.completed_source_epoch.is_none());
    assert!(!refreshed.matches_run(&after, LEXICAL_REBUILD_PAGE_SIZE));
    assert!(!published_lexical_index_validated_for_current_data(
        &fixture.index_path,
        &fixture.db_path
    ));
    // Refusing certification does not destroy the readable prior generation.
    assert_eq!(
        gh496_search_signature(&fixture.index_path, "finalword0z0").len(),
        1
    );
    assert!(gh496_search_signature(&fixture.index_path, "legacycompletionrewrite").is_empty());
    drop(storage);

    rebuild_tantivy_from_db(&fixture.db_path, &fixture.data_dir, 2, None).unwrap();
    gh496_assert_exact(&fixture, 2);
    assert_eq!(
        gh496_search_signature(&fixture.index_path, "legacycompletionrewrite").len(),
        1
    );
}

#[test]
#[serial_test::serial]
fn gh496_count_refresh_cannot_complete_or_overwrite_an_incomplete_staged_audit() {
    let fixture = gh496_fixture();
    let before = load_lexical_rebuild_state(&fixture.index_path)
        .unwrap()
        .unwrap();
    assert!(before.is_incomplete());
    let scratch = staged_lexical_rebuild_scratch_path(&fixture.index_path);
    let candidate_manifest = fs::read(scratch.join("MANIFEST")).unwrap();
    let storage = FrankenStorage::open_strict_readonly(&fixture.db_path).unwrap();
    let db_state = lexical_rebuild_db_state(&storage, &fixture.db_path).unwrap();
    let observed_docs = live_tantivy_doc_count(&fixture.index_path).unwrap();
    assert_eq!(observed_docs, Some(db_state.total_messages));
    assert!(
        !persist_completed_lexical_rebuild_checkpoint_from_observations(
            &storage,
            &fixture.index_path,
            db_state.clone(),
            db_state.total_messages,
            max_conversation_id_exact(&storage).unwrap(),
            observed_docs,
            true,
        )
        .unwrap(),
        "the prior live count cannot certify the separately staged candidate"
    );
    assert_eq!(
        load_lexical_rebuild_state(&fixture.index_path)
            .unwrap()
            .unwrap(),
        before
    );
    assert_eq!(
        fs::read(scratch.join("MANIFEST")).unwrap(),
        candidate_manifest
    );
}

#[test]
#[serial_test::serial]
fn gh496_metadata_epoch_drift_is_revalidated_without_native_writes() {
    let fixture = gh496_fixture();
    rebuild_tantivy_from_db(&fixture.db_path, &fixture.data_dir, 2, None).unwrap();
    let before = load_lexical_rebuild_state(&fixture.index_path)
        .unwrap()
        .unwrap();
    let storage = FrankenStorage::open(&fixture.db_path).unwrap();
    storage
        .set_last_indexed_at(FrankenStorage::now_millis())
        .unwrap();
    storage.close_without_checkpoint().unwrap();
    assert!(!before.completed_source_matches(&fixture.db_path));
    let manifest = fs::read(fixture.index_path.join("MANIFEST")).unwrap();
    let segments = gh496_segments(&fixture.index_path);
    let mut observed_messages = 0;
    let mut committed_batches = 0;
    let result = lexical_resume::revalidate_completed_with_observer(
        &fixture.db_path,
        &fixture.index_path,
        before,
        None,
        None,
        |event| {
            match event {
                lexical_resume::Event::CanonicalMessage { .. } => observed_messages += 1,
                lexical_resume::Event::CommittedBatch { .. } => committed_batches += 1,
                lexical_resume::Event::BeforePublication => {}
            }
            Ok(())
        },
    )
    .unwrap()
    .expect("unchanged canonical projections must revalidate");
    assert_eq!(result.indexed_docs, 4);
    assert_eq!(observed_messages, 4);
    assert_eq!(
        committed_batches, 0,
        "completion audit must not rewrite native documents"
    );
    assert_eq!(
        fs::read(fixture.index_path.join("MANIFEST")).unwrap(),
        manifest
    );
    gh496_assert_segments_retained(&fixture.index_path, &segments);
    let after = load_lexical_rebuild_state(&fixture.index_path)
        .unwrap()
        .unwrap();
    assert!(after.completed_source_matches(&fixture.db_path));
    assert!(published_lexical_index_validated_for_current_data(
        &fixture.index_path,
        &fixture.db_path
    ));
    gh496_assert_exact(&fixture, 2);
}

#[test]
#[serial_test::serial]
fn gh496_completed_revalidation_does_not_absorb_a_raw_writer() {
    let fixture = gh496_fixture();
    rebuild_tantivy_from_db(&fixture.db_path, &fixture.data_dir, 2, None).unwrap();
    let before = load_lexical_rebuild_state(&fixture.index_path)
        .unwrap()
        .unwrap();
    let manifest = fs::read(fixture.index_path.join("MANIFEST")).unwrap();
    let writer = FrankenStorage::open(&fixture.db_path).unwrap();
    writer
        .set_last_indexed_at(FrankenStorage::now_millis())
        .unwrap();
    let mut changed = false;
    let error = lexical_resume::revalidate_completed_with_observer(
        &fixture.db_path,
        &fixture.index_path,
        before.clone(),
        None,
        None,
        |event| {
            if let lexical_resume::Event::BeforePublication = event {
                writer.raw().execute(
                    "UPDATE messages SET content = 'growthproof racedcompletionaudit' \
                     WHERE conversation_id = 1 AND idx = 0",
                )?;
                changed = true;
            }
            Ok(())
        },
    )
    .expect_err("a source write after verification must not renew completion authority");
    assert!(changed);
    assert!(error.is::<lexical_resume::SourceChanged>(), "{error:#}");
    assert_eq!(
        load_lexical_rebuild_state(&fixture.index_path)
            .unwrap()
            .unwrap(),
        before
    );
    assert_eq!(
        fs::read(fixture.index_path.join("MANIFEST")).unwrap(),
        manifest
    );
    assert!(!published_lexical_index_validated_for_current_data(
        &fixture.index_path,
        &fixture.db_path
    ));
    drop(writer);
    rebuild_tantivy_from_db(&fixture.db_path, &fixture.data_dir, 2, None).unwrap();
    gh496_assert_exact(&fixture, 2);
    assert_eq!(
        gh496_search_signature(&fixture.index_path, "racedcompletionaudit").len(),
        1
    );
}

#[test]
#[serial_test::serial]
fn gh496_count_refresh_clears_proof_when_same_count_native_generation_changes() {
    let fixture = gh496_fixture();
    rebuild_tantivy_from_db(&fixture.db_path, &fixture.data_dir, 2, None).unwrap();
    let before = load_lexical_rebuild_state(&fixture.index_path)
        .unwrap()
        .unwrap();
    assert!(before.completed_source_epoch.is_some());
    let mut documents = gh496_documents(&fixture.db_path);
    documents[0].content = "growthproof substitutednativebody".into();
    let mut index = TantivyIndex::open_or_create(&fixture.index_path).unwrap();
    index
        .upsert_prebuilt_documents_slice(&documents[..1])
        .unwrap();
    index.commit().unwrap();
    assert_eq!(index.doc_count().unwrap(), 4);
    drop(index);
    assert!(!published_lexical_index_validated_for_current_data(
        &fixture.index_path,
        &fixture.db_path
    ));
    let storage = FrankenStorage::open_strict_readonly(&fixture.db_path).unwrap();
    let db_state = lexical_rebuild_db_state(&storage, &fixture.db_path).unwrap();
    assert!(
        !matching_lexical_rebuild_state_status(&fixture.index_path, &db_state)
            .unwrap()
            .has_completed_checkpoint
    );
    assert!(
        !matching_completed_lexical_rebuild_state_status_without_fingerprint(
            &fixture.index_path,
            &fixture.db_path,
            db_state.total_conversations,
        )
        .unwrap()
        .unwrap()
        .has_completed_checkpoint
    );
    refresh_completed_lexical_rebuild_checkpoint(&storage, &fixture.db_path, &fixture.data_dir)
        .unwrap();
    storage.close_without_checkpoint().unwrap();
    let refreshed = load_lexical_rebuild_state(&fixture.index_path)
        .unwrap()
        .unwrap();
    assert!(
        refreshed.completed_source_epoch.is_none(),
        "a new native manifest cannot inherit an older source proof"
    );
    assert!(!published_lexical_index_validated_for_current_data(
        &fixture.index_path,
        &fixture.db_path
    ));
    rebuild_tantivy_from_db(&fixture.db_path, &fixture.data_dir, 2, None).unwrap();
    gh496_assert_exact(&fixture, 2);
    assert!(gh496_search_signature(&fixture.index_path, "substitutednativebody").is_empty());
    assert_eq!(
        gh496_search_signature(&fixture.index_path, "finalword0z0").len(),
        1
    );
}

fn gh496_assert_completed_artifact_pair(
    fixture: &Gh496Fixture,
    conversations: usize,
    messages: usize,
    documents: usize,
) -> super::LexicalRebuildEquivalenceEvidence {
    let state = load_lexical_rebuild_state(&fixture.index_path)
        .unwrap()
        .unwrap();
    let manifest = lexical_generation::load_manifest(&fixture.index_path)
        .unwrap()
        .unwrap();
    let evidence: super::LexicalRebuildEquivalenceEvidence = serde_json::from_slice(
        &fs::read(lexical_rebuild_equivalence_evidence_path(
            &fixture.index_path,
        ))
        .unwrap(),
    )
    .unwrap();
    assert!(state.completed);
    assert!(state.completed_source_matches(&fixture.db_path));
    assert_eq!(state.db.total_conversations, conversations);
    assert_eq!(state.db.total_messages, messages);
    assert_eq!(state.indexed_docs, documents);
    assert_eq!(manifest.source_db_fingerprint, state.db.storage_fingerprint);
    assert_eq!(manifest.conversation_count, conversations as u64);
    assert_eq!(manifest.message_count, messages as u64);
    assert_eq!(manifest.indexed_doc_count, documents as u64);
    assert_eq!(
        manifest.build_state,
        lexical_generation::LexicalGenerationBuildState::Validated
    );
    assert_eq!(
        manifest.publish_state,
        lexical_generation::LexicalGenerationPublishState::Published
    );
    assert_eq!(evidence.document_count, documents as u64);
    assert_eq!(
        manifest.equivalence_manifest_fingerprint.as_ref(),
        Some(&evidence.manifest_fingerprint)
    );
    // Counts and paired JSON alone cannot prove that this native generation
    // contains every canonical projection with its current stored content.
    gh496_assert_exact(fixture, conversations);
    evidence
}

#[test]
#[serial_test::serial]
fn gh496_completed_revalidation_refreshes_artifact_pair_after_unindexed_growth() {
    let fixture = gh496_fixture();
    rebuild_tantivy_from_db(&fixture.db_path, &fixture.data_dir, 2, None).unwrap();
    let before = load_lexical_rebuild_state(&fixture.index_path)
        .unwrap()
        .unwrap();
    let old_evidence = gh496_assert_completed_artifact_pair(&fixture, 2, 4, 4);
    let native_manifest = fs::read(fixture.index_path.join("MANIFEST")).unwrap();
    let native_fingerprint = index_meta_fingerprint(&fixture.index_path).unwrap();
    let segments = gh496_segments(&fixture.index_path);

    let storage = FrankenStorage::open(&fixture.db_path).unwrap();
    gh496_insert_conversation(&storage, 2, 0, 32);
    let noise_conversation = gh496_insert_conversation(&storage, 3, 2, 32);
    storage
        .raw()
        .execute(&format!(
            "UPDATE messages SET content = '   ' WHERE conversation_id = {noise_conversation}"
        ))
        .unwrap();
    storage.close_without_checkpoint().unwrap();
    assert_eq!(gh496_documents(&fixture.db_path).len(), 4);

    let mut observed_messages = 0;
    let mut committed_batches = 0;
    let outcome = lexical_resume::revalidate_completed_with_observer(
        &fixture.db_path,
        &fixture.index_path,
        before.clone(),
        None,
        None,
        |event| {
            match event {
                lexical_resume::Event::CanonicalMessage { .. } => observed_messages += 1,
                lexical_resume::Event::CommittedBatch { .. } => committed_batches += 1,
                lexical_resume::Event::BeforePublication => {}
            }
            Ok(())
        },
    )
    .unwrap()
    .expect("empty and noise-only growth preserves every native document");
    assert_eq!(observed_messages, 6);
    assert_eq!(committed_batches, 0);
    assert_eq!(outcome.observed_conversations, 4);
    assert_eq!(outcome.observed_messages, Some(6));
    assert_eq!(outcome.indexed_docs, 4);
    let evidence = gh496_assert_completed_artifact_pair(&fixture, 4, 6, 4);
    assert_eq!(outcome.equivalence.as_ref(), Some(&evidence));
    assert_ne!(
        evidence.manifest_fingerprint,
        old_evidence.manifest_fingerprint
    );
    let after = load_lexical_rebuild_state(&fixture.index_path)
        .unwrap()
        .unwrap();
    assert_ne!(after.db.storage_fingerprint, before.db.storage_fingerprint);
    let status = matching_lexical_rebuild_state_status(&fixture.index_path, &after.db).unwrap();
    assert_eq!(status.completed_exact_totals, Some((4, 6)));
    assert_eq!(status.completed_indexed_docs, Some(4));
    assert_eq!(
        fs::read(fixture.index_path.join("MANIFEST")).unwrap(),
        native_manifest
    );
    assert_eq!(
        index_meta_fingerprint(&fixture.index_path).unwrap(),
        native_fingerprint
    );
    gh496_assert_segments_retained(&fixture.index_path, &segments);
}

#[test]
#[serial_test::serial]
fn gh496_completed_revalidation_repairs_missing_or_corrupt_artifact_pair() {
    let fixture = gh496_fixture();
    rebuild_tantivy_from_db(&fixture.db_path, &fixture.data_dir, 2, None).unwrap();
    let native_manifest = fs::read(fixture.index_path.join("MANIFEST")).unwrap();
    let segments = gh496_segments(&fixture.index_path);
    for (name, path) in [
        (
            "generation",
            lexical_generation::manifest_path(&fixture.index_path),
        ),
        (
            "equivalence",
            lexical_rebuild_equivalence_evidence_path(&fixture.index_path),
        ),
    ] {
        for damage in ["missing", "corrupt"] {
            let before = load_lexical_rebuild_state(&fixture.index_path)
                .unwrap()
                .unwrap();
            assert!(before.completed_source_matches(&fixture.db_path));
            if damage == "missing" {
                fs::rename(
                    &path,
                    fixture._tmp.path().join(format!("held-{name}-artifact")),
                )
                .unwrap();
            } else {
                fs::write(&path, b"{ interrupted paired artifact").unwrap();
            }
            let mut observed_messages = 0;
            let outcome = lexical_resume::revalidate_completed_with_observer(
                &fixture.db_path,
                &fixture.index_path,
                before,
                None,
                None,
                |event| {
                    match event {
                        lexical_resume::Event::CanonicalMessage { .. } => observed_messages += 1,
                        lexical_resume::Event::CommittedBatch { .. } => {
                            panic!("repairing completion metadata must not commit native documents")
                        }
                        lexical_resume::Event::BeforePublication => {}
                    }
                    Ok(())
                },
            )
            .unwrap()
            .expect("a verified native generation can repair its paired artifacts");
            assert_eq!(observed_messages, 4, "{name}/{damage} must force an audit");
            let evidence = gh496_assert_completed_artifact_pair(&fixture, 2, 4, 4);
            assert_eq!(outcome.equivalence.as_ref(), Some(&evidence));
            assert_eq!(
                fs::read(fixture.index_path.join("MANIFEST")).unwrap(),
                native_manifest
            );
            gh496_assert_segments_retained(&fixture.index_path, &segments);
        }
    }
}

#[test]
#[serial_test::serial]
fn gh496_completed_fast_path_rechecks_source_and_native_generation_before_acceptance() {
    for mutation in ["source", "native"] {
        let fixture = gh496_fixture();
        rebuild_tantivy_from_db(&fixture.db_path, &fixture.data_dir, 2, None).unwrap();
        gh496_assert_completed_artifact_pair(&fixture, 2, 4, 4);
        let before = load_lexical_rebuild_state(&fixture.index_path)
            .unwrap()
            .unwrap();
        let checkpoint_bytes = fs::read(lexical_rebuild_state_path(&fixture.index_path)).unwrap();
        let mut documents = gh496_documents(&fixture.db_path);
        documents[0].content = "growthproof changednativeduringadmission".into();

        let mut unchanged_admissions = 0;
        let unchanged = lexical_resume::revalidate_completed_with_observer(
            &fixture.db_path,
            &fixture.index_path,
            before.clone(),
            None,
            None,
            |event| {
                match event {
                    lexical_resume::Event::BeforePublication => unchanged_admissions += 1,
                    _ => panic!("unchanged completion must use the shortcut without a source scan"),
                }
                Ok(())
            },
        )
        .unwrap()
        .expect("unchanged completed evidence admits the shortcut");
        assert_eq!(unchanged_admissions, 1);
        assert!(!unchanged.exact_checkpoint_persisted);
        assert!(unchanged.equivalence.is_none());

        let mut admission_callbacks = 0;
        let mut observed_messages = 0;
        let error = lexical_resume::revalidate_completed_with_observer(
            &fixture.db_path,
            &fixture.index_path,
            before.clone(),
            None,
            None,
            |event| {
                match event {
                    lexical_resume::Event::CanonicalMessage { .. } => observed_messages += 1,
                    lexical_resume::Event::CommittedBatch { .. } => {
                        panic!("completed admission must not write native documents")
                    }
                    lexical_resume::Event::BeforePublication => {
                        admission_callbacks += 1;
                        if mutation == "source" {
                            let writer = FrankenStorage::open(&fixture.db_path)?;
                            writer.raw().execute(&format!(
                                "UPDATE messages SET content = 'growthproof changedsourceduringadmission' \
                                 WHERE conversation_id = {} AND idx = 0",
                                fixture.first_conversation_id,
                            ))?;
                            writer.close_without_checkpoint()?;
                        } else {
                            let mut writer = TantivyIndex::open_or_create(&fixture.index_path)?;
                            writer.upsert_prebuilt_documents_slice(&documents[..1])?;
                            writer.commit()?;
                            assert_eq!(writer.doc_count()?, 4);
                        }
                    }
                }
                Ok(())
            },
        )
        .expect_err("a change after initial admission must invalidate the shortcut");
        assert_eq!(admission_callbacks, 1);
        assert_eq!(observed_messages, 0, "the fast path must be exercised");
        if mutation == "source" {
            assert!(error.is::<lexical_resume::SourceChanged>(), "{error:#}");
        } else {
            assert!(
                format!("{error:#}")
                    .contains("native lexical generation changed during completed admission"),
                "{error:#}"
            );
        }
        assert_eq!(
            fs::read(lexical_rebuild_state_path(&fixture.index_path)).unwrap(),
            checkpoint_bytes
        );
        assert_eq!(
            load_lexical_rebuild_state(&fixture.index_path)
                .unwrap()
                .unwrap(),
            before
        );
        assert!(!published_lexical_index_validated_for_current_data(
            &fixture.index_path,
            &fixture.db_path
        ));
    }
}
