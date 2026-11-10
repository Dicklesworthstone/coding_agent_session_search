// Included by indexer::tests. An interrupted explicit forced rebuild
// (--force-rebuild) continues only its own candidate. These tests stop the
// production rebuild at a chosen point on a real canonical archive, rerun it,
// and compare the result with an uninterrupted forced rebuild of that archive.

struct ForcedResumeFixture {
    _tmp: TempDir,
    root: PathBuf,
    db_path: PathBuf,
    conversations: usize,
}

fn forced_resume_fixture(
    conversations: usize,
    messages: usize,
    body_bytes: usize,
) -> ForcedResumeFixture {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();
    let db_path = root.join("agent_search.db");
    let storage = FrankenStorage::open(&db_path).unwrap();
    ensure_fts_schema(&storage);
    for sequence in 0..conversations {
        gh496_insert_conversation(&storage, sequence, messages, body_bytes);
    }
    drop(storage);
    ForcedResumeFixture {
        _tmp: tmp,
        root,
        db_path,
        conversations,
    }
}

/// The settings one forced-resume test runs under, restored when dropped.
struct ForcedResumeEnv {
    _env: Vec<EnvGuard>,
    _fold_cap: crate::search::quill_bridge::LexicalMergeCapTestGuard,
}

/// Small commits and pages, so one rebuild of a small archive commits several
/// segments before its fold, and an explicit fold run cap. The cap is set for
/// this test thread only, where the rebuild plans its fold: through the
/// process environment it also split the folds of unrelated tests running
/// beside this one.
fn forced_resume_env(commit_every_conversations: usize, fold_cap_bytes: u64) -> ForcedResumeEnv {
    let commit = commit_every_conversations.to_string();
    ForcedResumeEnv {
        _env: vec![
            set_env("CASS_RESPONSIVENESS_DISABLE", "1"),
            set_env("CASS_TANTIVY_REBUILD_COMMIT_EVERY_CONVERSATIONS", &commit),
            set_env(
                "CASS_TANTIVY_REBUILD_INITIAL_COMMIT_EVERY_CONVERSATIONS",
                &commit,
            ),
            set_env("CASS_TANTIVY_REBUILD_BATCH_FETCH_CONVERSATIONS", "2"),
            set_env(
                "CASS_TANTIVY_REBUILD_INITIAL_BATCH_FETCH_CONVERSATIONS",
                "2",
            ),
        ],
        _fold_cap: crate::search::quill_bridge::lexical_merge_max_output_bytes_for_test(
            fold_cap_bytes,
        ),
    }
}

/// One explicit forced rebuild (the `cass index --force-rebuild` call), with
/// an optional stop, returning its result and the documents it added.
fn forced_resume_rebuild(
    fixture: &ForcedResumeFixture,
    data_dir: &Path,
    interrupt: Option<LexicalRebuildTestInterrupt>,
) -> (Result<LexicalRebuildOutcome>, usize) {
    fs::create_dir_all(data_dir).unwrap();
    LEXICAL_REBUILD_TEST_INTERRUPT.with(|cell| cell.set(interrupt));
    LEXICAL_REBUILD_TEST_ADDED_DOCS.with(|cell| cell.set(0));
    let outcome = rebuild_tantivy_from_db_with_options(
        &fixture.db_path,
        data_dir,
        fixture.conversations,
        None,
        LexicalRebuildStartupOptions {
            defer_initial_content_fingerprint: true,
            force_fresh: true,
        },
        None,
    );
    LEXICAL_REBUILD_TEST_INTERRUPT.with(|cell| cell.set(None));
    (
        outcome,
        LEXICAL_REBUILD_TEST_ADDED_DOCS.with(std::cell::Cell::get),
    )
}

fn forced_resume_interrupted(
    fixture: &ForcedResumeFixture,
    data_dir: &Path,
    interrupt: LexicalRebuildTestInterrupt,
) -> LexicalRebuildState {
    let (outcome, _) = forced_resume_rebuild(fixture, data_dir, Some(interrupt));
    let error = outcome.expect_err("the rebuild must stop where the test interrupts it");
    assert!(format!("{error:#}").contains("test interrupt"), "{error:#}");
    let checkpoint = load_lexical_rebuild_state(&index_dir(data_dir).unwrap())
        .unwrap()
        .expect("an interrupted rebuild keeps its checkpoint");
    assert!(checkpoint.is_incomplete());
    checkpoint
}

fn forced_resume_quarantines(data_dir: &Path) -> usize {
    let parent = index_dir(data_dir).unwrap().parent().unwrap().to_path_buf();
    fs::read_dir(parent)
        .unwrap()
        .map(Result::unwrap)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".lexical-rebuild-quarantine-")
        })
        .count()
}

/// `gh496_search_signature` returns at most this many hits. Past it, equally
/// scored hits are cut by document order, which segment layout decides.
const FORCED_RESUME_SIGNATURE_HITS: usize = 1000;

/// Each query's complete answer is compared, so a query is used only while its
/// answer fits in one signature: per-conversation terms always, terms in
/// every conversation or document only on archives that small.
fn forced_resume_queries(conversations: usize, documents: usize) -> Vec<&'static str> {
    let mut queries = vec!["stableword0", "stableword23", "finalword3z2"];
    if conversations < FORCED_RESUME_SIGNATURE_HITS {
        queries.push("messageword1");
    }
    if documents < FORCED_RESUME_SIGNATURE_HITS {
        queries.extend(["growthproof", "canonical"]);
    }
    queries
}

/// Same documents, same search answers and the same accumulated proofs as the
/// uninterrupted forced rebuild of the same archive.
fn assert_forced_resume_matches_uninterrupted(
    fixture: &ForcedResumeFixture,
    reference_dir: &Path,
    reference: &LexicalRebuildOutcome,
    data_dir: &Path,
    outcome: &LexicalRebuildOutcome,
) {
    assert_eq!(
        outcome.observed_conversations,
        reference.observed_conversations
    );
    assert_eq!(outcome.indexed_docs, reference.indexed_docs);
    assert_eq!(outcome.observed_messages, reference.observed_messages);
    assert!(outcome.equivalence.is_some());
    assert_eq!(
        outcome.equivalence, reference.equivalence,
        "the resumed run must absorb the uninterrupted run's packet stream"
    );
    let index_path = index_dir(data_dir).unwrap();
    let reference_index = index_dir(reference_dir).unwrap();
    verify_published_lexical_doc_count(&index_path, reference.indexed_docs, "forced resume")
        .unwrap();
    let completed = load_lexical_rebuild_state(&index_path).unwrap().unwrap();
    let expected = load_lexical_rebuild_state(&reference_index)
        .unwrap()
        .unwrap();
    assert!(completed.completed);
    assert_eq!(completed.db, expected.db);
    assert_eq!(completed.indexed_docs, expected.indexed_docs);
    assert_eq!(
        completed.committed_conversation_id,
        expected.committed_conversation_id
    );
    assert!(completed.forced_rebuild_origin.is_none());
    // Completed checkpoints and receipts keep the format older readers parse.
    for file in [
        lexical_rebuild_state_path(&index_path),
        index_path.join(LEXICAL_PUBLISHED_STATE_FILE),
    ] {
        assert!(
            !fs::read_to_string(&file)
                .unwrap()
                .contains("forced_rebuild_origin"),
            "{}",
            file.display()
        );
    }
    // Every canonical document is present with its complete projection, and
    // nothing else is.
    let audited = lexical_resume::audit_with_classify_pool(
        &fixture.db_path,
        &index_path,
        lexical_reconcile::canary::exact::PublishedSnapshot::open(&index_path).unwrap(),
        None,
    )
    .unwrap();
    assert_eq!(audited.0, reference.indexed_docs);
    assert_eq!(audited.1, reference.indexed_docs);
    for query in forced_resume_queries(fixture.conversations, reference.indexed_docs) {
        let hits = gh496_search_signature(&index_path, query);
        assert!(
            !hits.is_empty() && hits.len() < FORCED_RESUME_SIGNATURE_HITS,
            "{query} must have a complete answer on the fixture"
        );
        assert_eq!(
            hits,
            gh496_search_signature(&reference_index, query),
            "resumed and uninterrupted forced rebuilds disagree for {query}"
        );
    }
}

#[test]
#[serial_test::serial]
fn forced_rebuild_resumes_its_own_interrupted_ingest_without_readding_the_prefix() {
    let _env = forced_resume_env(4, 32 * 1024 * 1024);
    let fixture = forced_resume_fixture(24, 3, 96);
    let reference_dir = fixture.root.join("uninterrupted");
    let (reference, reference_added) = forced_resume_rebuild(&fixture, &reference_dir, None);
    let reference = reference.unwrap();
    assert_eq!(reference_added, reference.indexed_docs);

    let data_dir = fixture.root.join("resumed");
    let checkpoint = forced_resume_interrupted(
        &fixture,
        &data_dir,
        LexicalRebuildTestInterrupt::AfterCommits(2),
    );
    assert!(checkpoint.forced_rebuild_origin.is_some());
    assert_eq!(
        checkpoint.execution_mode,
        Some(LexicalRebuildExecutionMode::StagedSingleIndex)
    );
    let committed_docs = checkpoint.indexed_docs;
    assert!(committed_docs > 0 && committed_docs < reference.indexed_docs);

    let (outcome, added) = forced_resume_rebuild(&fixture, &data_dir, None);
    let outcome = outcome.unwrap();
    assert_eq!(
        added,
        reference.indexed_docs - committed_docs,
        "a resumed forced rebuild adds only what its checkpoint had not committed"
    );
    assert_eq!(forced_resume_quarantines(&data_dir), 0);
    assert_forced_resume_matches_uninterrupted(
        &fixture,
        &reference_dir,
        &reference,
        &data_dir,
        &outcome,
    );
}

#[test]
#[serial_test::serial]
fn forced_rebuild_stopped_during_its_fold_resumes_the_fold_without_reingesting() {
    let _env = forced_resume_env(4, 32 * 1024 * 1024);
    let fixture = forced_resume_fixture(24, 3, 96);
    let reference_dir = fixture.root.join("uninterrupted");
    let reference = forced_resume_rebuild(&fixture, &reference_dir, None)
        .0
        .unwrap();

    let data_dir = fixture.root.join("resumed");
    let checkpoint =
        forced_resume_interrupted(&fixture, &data_dir, LexicalRebuildTestInterrupt::DuringFold);
    assert!(checkpoint.forced_rebuild_origin.is_some());
    assert_eq!(checkpoint.processed_conversations, fixture.conversations);
    assert_eq!(checkpoint.indexed_docs, reference.indexed_docs);
    assert!(checkpoint.pending.is_none());
    let scratch = staged_lexical_rebuild_scratch_path(&index_dir(&data_dir).unwrap());
    assert_ne!(
        index_meta_fingerprint(&scratch).unwrap(),
        checkpoint.committed_meta_fingerprint,
        "a fold publication must have landed after the final ingest checkpoint"
    );

    let (outcome, added) = forced_resume_rebuild(&fixture, &data_dir, None);
    let outcome = outcome.unwrap();
    assert_eq!(
        added, 0,
        "a stop during the fold must not re-ingest anything"
    );
    assert_eq!(forced_resume_quarantines(&data_dir), 0);
    assert_forced_resume_matches_uninterrupted(
        &fixture,
        &reference_dir,
        &reference,
        &data_dir,
        &outcome,
    );
}

#[test]
#[serial_test::serial]
fn forced_rebuild_starts_over_when_the_archive_changed_after_the_interruption() {
    let _env = forced_resume_env(4, 32 * 1024 * 1024);
    let mut fixture = forced_resume_fixture(24, 3, 96);
    let data_dir = fixture.root.join("resumed");
    let checkpoint = forced_resume_interrupted(
        &fixture,
        &data_dir,
        LexicalRebuildTestInterrupt::AfterCommits(2),
    );
    assert!(checkpoint.indexed_docs > 0);

    // Any archive write moves the write epoch the forced run started from.
    let storage = FrankenStorage::open(&fixture.db_path).unwrap();
    gh496_insert_conversation(&storage, fixture.conversations, 3, 96);
    drop(storage);
    fixture.conversations += 1;

    let (outcome, added) = forced_resume_rebuild(&fixture, &data_dir, None);
    let outcome = outcome.unwrap();
    assert_eq!(added, outcome.indexed_docs, "every document is rebuilt");
    assert_eq!(forced_resume_quarantines(&data_dir), 1);
    let reference_dir = fixture.root.join("uninterrupted");
    let reference = forced_resume_rebuild(&fixture, &reference_dir, None)
        .0
        .unwrap();
    assert_forced_resume_matches_uninterrupted(
        &fixture,
        &reference_dir,
        &reference,
        &data_dir,
        &outcome,
    );
}

#[test]
#[serial_test::serial]
fn forced_rebuild_never_continues_a_checkpoint_it_did_not_start() {
    let _env = forced_resume_env(4, 32 * 1024 * 1024);
    let fixture = forced_resume_fixture(24, 3, 96);
    let reference_dir = fixture.root.join("uninterrupted");
    let reference = forced_resume_rebuild(&fixture, &reference_dir, None)
        .0
        .unwrap();

    // An ordinary rebuild leaves the same staged, archive-bound candidate,
    // without a forced origin.
    let ordinary_dir = fixture.root.join("ordinary");
    fs::create_dir_all(&ordinary_dir).unwrap();
    LEXICAL_REBUILD_TEST_INTERRUPT
        .with(|cell| cell.set(Some(LexicalRebuildTestInterrupt::AfterCommits(2))));
    let error = rebuild_tantivy_from_db_with_options(
        &fixture.db_path,
        &ordinary_dir,
        fixture.conversations,
        None,
        LexicalRebuildStartupOptions {
            defer_initial_content_fingerprint: true,
            force_fresh: false,
        },
        None,
    )
    .expect_err("interrupted ordinary rebuild");
    LEXICAL_REBUILD_TEST_INTERRUPT.with(|cell| cell.set(None));
    assert!(format!("{error:#}").contains("test interrupt"), "{error:#}");
    let ordinary = load_lexical_rebuild_state(&index_dir(&ordinary_dir).unwrap())
        .unwrap()
        .unwrap();
    assert!(ordinary.is_incomplete() && ordinary.indexed_docs > 0);
    assert!(ordinary.resume_binding.is_some());
    assert!(ordinary.forced_rebuild_origin.is_none());

    // A forced checkpoint from another cass build is not continued either.
    let foreign_dir = fixture.root.join("foreign");
    let mut foreign = forced_resume_interrupted(
        &fixture,
        &foreign_dir,
        LexicalRebuildTestInterrupt::AfterCommits(2),
    );
    foreign.forced_rebuild_origin.as_mut().unwrap().cass_version = "0.0.0-other".to_string();
    persist_lexical_rebuild_state(&index_dir(&foreign_dir).unwrap(), &foreign).unwrap();

    for data_dir in [&ordinary_dir, &foreign_dir] {
        let (outcome, added) = forced_resume_rebuild(&fixture, data_dir, None);
        let outcome = outcome.unwrap();
        assert_eq!(added, reference.indexed_docs, "every document is rebuilt");
        assert_eq!(forced_resume_quarantines(data_dir), 1);
        assert_forced_resume_matches_uninterrupted(
            &fixture,
            &reference_dir,
            &reference,
            data_dir,
            &outcome,
        );
    }
}

/// `cass index --force-rebuild` (or with `--full`) over an existing archive,
/// with no source roots.
fn forced_resume_index(fixture: &ForcedResumeFixture, data_dir: &Path, full: bool) -> Result<()> {
    fs::create_dir_all(data_dir).unwrap();
    LEXICAL_REBUILD_TEST_ADDED_DOCS.with(|cell| cell.set(0));
    run_index_with_local_connector_roots(
        IndexOptions {
            full,
            force_rebuild: true,
            watch: false,
            watch_once_paths: None,
            db_path: fixture.db_path.clone(),
            data_dir: data_dir.to_path_buf(),
            semantic: false,
            build_hnsw: false,
            embedder: "hash".into(),
            progress: Some(Arc::new(IndexingProgress::default())),
            watch_interval_secs: 30,
        },
        HashMap::new(),
        None,
    )
}

/// The production `cass index --force-rebuild` entry (its readonly archive
/// preflight included) continues its own interrupted run. `--full
/// --force-rebuild` writes a fresh checkpoint before its rebuild, so after the
/// same interruption it still rebuilds every document.
#[test]
#[serial_test::serial]
fn forced_index_command_resumes_its_own_interrupted_rebuild() {
    let _env = forced_resume_env(4, 32 * 1024 * 1024);
    let fixture = forced_resume_fixture(24, 3, 96);
    let reference_dir = fixture.root.join("uninterrupted");
    let reference = forced_resume_rebuild(&fixture, &reference_dir, None)
        .0
        .unwrap();
    let reference_index = index_dir(&reference_dir).unwrap();

    for (rerun_full, data_dir) in [
        (false, fixture.root.join("forced")),
        (true, fixture.root.join("full-forced")),
    ] {
        LEXICAL_REBUILD_TEST_INTERRUPT
            .with(|cell| cell.set(Some(LexicalRebuildTestInterrupt::AfterCommits(2))));
        let error = forced_resume_index(&fixture, &data_dir, false)
            .expect_err("the forced index must stop where the test interrupts it");
        LEXICAL_REBUILD_TEST_INTERRUPT.with(|cell| cell.set(None));
        assert!(format!("{error:#}").contains("test interrupt"), "{error:#}");
        let index_path = index_dir(&data_dir).unwrap();
        let checkpoint = load_lexical_rebuild_state(&index_path).unwrap().unwrap();
        assert!(checkpoint.is_incomplete() && checkpoint.forced_rebuild_origin.is_some());
        let committed_docs = checkpoint.indexed_docs;
        assert!(committed_docs > 0 && committed_docs < reference.indexed_docs);

        forced_resume_index(&fixture, &data_dir, rerun_full).unwrap();
        let added = LEXICAL_REBUILD_TEST_ADDED_DOCS.with(std::cell::Cell::get);
        if rerun_full {
            assert_eq!(
                added, reference.indexed_docs,
                "--full rebuilds every document"
            );
            assert_eq!(forced_resume_quarantines(&data_dir), 1);
        } else {
            assert_eq!(
                added,
                reference.indexed_docs - committed_docs,
                "the rerun adds only what the interrupted forced index had not committed"
            );
            assert_eq!(forced_resume_quarantines(&data_dir), 0);
        }
        let completed = load_lexical_rebuild_state(&index_path).unwrap().unwrap();
        assert!(completed.completed && completed.forced_rebuild_origin.is_none());
        assert_eq!(completed.indexed_docs, reference.indexed_docs);
        verify_published_lexical_doc_count(&index_path, reference.indexed_docs, "forced resume")
            .unwrap();
        for query in forced_resume_queries(fixture.conversations, reference.indexed_docs) {
            assert_eq!(
                gh496_search_signature(&index_path, query),
                gh496_search_signature(&reference_index, query),
                "resumed and uninterrupted forced rebuilds disagree for {query}"
            );
        }
    }
}

fn forced_resume_ab_setting(key: &str, default: usize) -> usize {
    dotenvy::var(key)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn forced_resume_percentile(values: &[f64], quantile: f64) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = ((sorted.len() - 1) as f64 * quantile).round() as usize;
    sorted[rank]
}

/// Rerun cost after an interrupted forced rebuild, incumbent (forced runs
/// never resume) against candidate (a forced run resumes its own candidate),
/// in one binary. Each arm reruns its own copy of the same deterministic
/// interrupted run (stopped at the same commit or fold run), so neither arm
/// reopens files another arm wrote. Both arms are checked against an
/// uninterrupted forced rebuild.
///
/// `cargo test --release --lib forced_resume_interrupt_rerun_ab -- --ignored --nocapture`
/// Scale: CASS_FORCED_RESUME_AB_{CONVERSATIONS,MESSAGES,BODY_BYTES,COMMIT_EVERY,FOLD_CAP_MIB,ROUNDS}.
#[test]
#[ignore = "A/B harness: run explicitly with --ignored --nocapture"]
#[serial_test::serial]
fn forced_resume_interrupt_rerun_ab() {
    let conversations = forced_resume_ab_setting("CASS_FORCED_RESUME_AB_CONVERSATIONS", 2_000);
    let messages = forced_resume_ab_setting("CASS_FORCED_RESUME_AB_MESSAGES", 24);
    let body_bytes = forced_resume_ab_setting("CASS_FORCED_RESUME_AB_BODY_BYTES", 768);
    let commit_every = forced_resume_ab_setting("CASS_FORCED_RESUME_AB_COMMIT_EVERY", 100);
    let fold_cap_mib = forced_resume_ab_setting("CASS_FORCED_RESUME_AB_FOLD_CAP_MIB", 64);
    let rounds = forced_resume_ab_setting("CASS_FORCED_RESUME_AB_ROUNDS", 5);
    let _env = forced_resume_env(commit_every, fold_cap_mib as u64 * 1024 * 1024);
    let fixture = forced_resume_fixture(conversations, messages, body_bytes);
    let commits = conversations.div_ceil(commit_every);
    let phases = [
        (
            "mid_ingest",
            LexicalRebuildTestInterrupt::AfterCommits((commits / 2).max(1)),
        ),
        ("mid_fold", LexicalRebuildTestInterrupt::DuringFold),
    ];
    eprintln!(
        "FORCED_RESUME_AB scale conversations={conversations} messages_per_conversation={messages} body_bytes={body_bytes} commit_every={commit_every} fold_cap_mib={fold_cap_mib} rounds={rounds}"
    );
    for (phase, interrupt) in phases {
        let mut uninterrupted_ms = Vec::new();
        let mut incumbent_ms = Vec::new();
        let mut candidate_ms = Vec::new();
        let mut ratios = Vec::new();
        let mut incumbent_added = 0;
        let mut candidate_added = 0;
        for round in 0..rounds {
            let round_dir = TempDir::new_in(&fixture.root).unwrap();
            let reference_dir = round_dir.path().join("uninterrupted");
            let started = Instant::now();
            let (reference, _) = forced_resume_rebuild(&fixture, &reference_dir, None);
            uninterrupted_ms.push(started.elapsed().as_secs_f64() * 1000.0);
            let reference = reference.unwrap();

            let incumbent_dir = round_dir.path().join("incumbent");
            let candidate_dir = round_dir.path().join("candidate");
            let checkpoint = forced_resume_interrupted(&fixture, &incumbent_dir, interrupt);
            let candidate_checkpoint =
                forced_resume_interrupted(&fixture, &candidate_dir, interrupt);
            assert_eq!(candidate_checkpoint.indexed_docs, checkpoint.indexed_docs);
            assert_eq!(
                candidate_checkpoint.committed_conversation_id,
                checkpoint.committed_conversation_id
            );

            let run_arm = |incumbent: bool| {
                FORCED_LEXICAL_RESUME_DISABLED_FOR_TEST.with(|cell| cell.set(incumbent));
                let data_dir = if incumbent {
                    &incumbent_dir
                } else {
                    &candidate_dir
                };
                let started = Instant::now();
                let (outcome, added) = forced_resume_rebuild(&fixture, data_dir, None);
                let elapsed = started.elapsed().as_secs_f64() * 1000.0;
                FORCED_LEXICAL_RESUME_DISABLED_FOR_TEST.with(|cell| cell.set(false));
                let outcome = outcome.unwrap();
                assert_forced_resume_matches_uninterrupted(
                    &fixture,
                    &reference_dir,
                    &reference,
                    data_dir,
                    &outcome,
                );
                eprintln!(
                    "FORCED_RESUME_AB phase={phase} round={round} arm={} rerun_ms={elapsed:.1} added_docs={added} checkpoint_docs={} total_docs={}",
                    if incumbent { "incumbent" } else { "candidate" },
                    checkpoint.indexed_docs,
                    reference.indexed_docs,
                );
                (elapsed, added)
            };
            // Alternate which arm runs first.
            let ((inc_ms, inc_added), (cand_ms, cand_added)) = if round.is_multiple_of(2) {
                let incumbent = run_arm(true);
                (incumbent, run_arm(false))
            } else {
                let candidate = run_arm(false);
                (run_arm(true), candidate)
            };
            incumbent_ms.push(inc_ms);
            candidate_ms.push(cand_ms);
            ratios.push(cand_ms / inc_ms);
            incumbent_added = inc_added;
            candidate_added = cand_added;
        }
        eprintln!(
            "FORCED_RESUME_AB summary phase={phase} uninterrupted_p50_ms={:.1} incumbent_p50_ms={:.1} [{:.1}, {:.1}] candidate_p50_ms={:.1} [{:.1}, {:.1}] candidate/incumbent_p50={:.3} [{:.3}, {:.3}] incumbent_added_docs={incumbent_added} candidate_added_docs={candidate_added}",
            forced_resume_percentile(&uninterrupted_ms, 0.5),
            forced_resume_percentile(&incumbent_ms, 0.5),
            forced_resume_percentile(&incumbent_ms, 0.0),
            forced_resume_percentile(&incumbent_ms, 1.0),
            forced_resume_percentile(&candidate_ms, 0.5),
            forced_resume_percentile(&candidate_ms, 0.0),
            forced_resume_percentile(&candidate_ms, 1.0),
            forced_resume_percentile(&ratios, 0.5),
            forced_resume_percentile(&ratios, 0.0),
            forced_resume_percentile(&ratios, 1.0),
        );
    }
}
