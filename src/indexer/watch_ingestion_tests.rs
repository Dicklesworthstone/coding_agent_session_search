//! GH538: source receipts must bound watch retries without hiding unpublished
//! content. These tests use real connector parsing, canonical transactions and
//! Quill publication; the active-source control holds a real advisory lock.

use super::*;
use serial_test::serial;
use tempfile::TempDir;

struct Fixture {
    _temp: TempDir,
    kind: ConnectorKind,
    root: ScanRoot,
    options: IndexOptions,
    index_path: PathBuf,
    previous_active_skip: bool,
}

impl Fixture {
    fn new(kind: ConnectorKind) -> Result<Self> {
        let temp = TempDir::new()?;
        let data = temp.path().join("cass-data");
        fs::create_dir_all(&data)?;
        let root = ScanRoot::local(temp.path().join(match kind {
            ConnectorKind::Codex => ".codex/sessions",
            ConnectorKind::Claude => ".claude/projects/receipt-project",
            other => anyhow::bail!("unsupported watch fixture connector: {other:?}"),
        }));
        fs::create_dir_all(&root.path)?;
        let index_path = index_dir(&data)?;
        let options = IndexOptions {
            full: false,
            watch: true,
            force_rebuild: false,
            db_path: data.join("agent_search.db"),
            data_dir: data,
            semantic: false,
            build_hnsw: false,
            embedder: "hash".into(),
            progress: None,
            watch_once_paths: None,
            watch_interval_secs: 1,
        };
        Ok(Self {
            _temp: temp,
            kind,
            root,
            options,
            index_path,
            previous_active_skip: ACTIVE_SESSION_SOURCE_SKIP_OBSERVED
                .swap(false, Ordering::Relaxed),
        })
    }

    fn path(&self, id: &str) -> PathBuf {
        self.root.path.join(format!("rollout-{id}.jsonl"))
    }

    fn source(&self, id: &str, text: &str) -> Result<PathBuf> {
        let path = self.path(id);
        let mut file = File::create(&path)?;
        if self.kind == ConnectorKind::Codex {
            writeln!(
                file,
                "{}",
                serde_json::json!({
                    "timestamp": "2025-09-30T15:42:00.000Z", "type": "session_meta",
                    "payload": {"id": id, "cwd": "/receipt-workspace"}
                })
            )?;
        }
        drop(file);
        self.append(&path, id, text)?;
        age_source(&path)?;
        Ok(path)
    }

    fn append(&self, path: &Path, id: &str, text: &str) -> Result<()> {
        let record = if self.kind == ConnectorKind::Codex {
            serde_json::json!({
                "timestamp": "2025-09-30T15:43:00.000Z", "type": "response_item",
                "payload": {"type": "message", "role": "user",
                    "content": [{"type": "input_text", "text": text}]}
            })
        } else {
            serde_json::json!({
                "timestamp": "2025-09-30T15:43:00.000Z", "type": "user",
                "sessionId": id, "uuid": format!("{id}-{text}"), "cwd": "/receipt-workspace",
                "message": {"role": "user", "content": text}
            })
        };
        let mut file = OpenOptions::new().append(true).open(path)?;
        writeln!(file, "{record}")?;
        Ok(())
    }

    fn roots(&self) -> [(ConnectorKind, ScanRoot); 1] {
        [(self.kind, self.root.clone())]
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        ACTIVE_SESSION_SOURCE_SKIP_OBSERVED.store(self.previous_active_skip, Ordering::Relaxed);
    }
}

struct Handles {
    storage: Mutex<FrankenStorage>,
    index: Mutex<Option<TantivyIndex>>,
    state: Mutex<HashMap<ConnectorKind, i64>>,
}

impl Handles {
    fn open(fixture: &Fixture) -> Result<Self> {
        Ok(Self {
            storage: Mutex::new(FrankenStorage::open(&fixture.options.db_path)?),
            index: Mutex::new(Some(TantivyIndex::open_or_create(&fixture.index_path)?)),
            state: Mutex::new(load_watch_state(&fixture.options.data_dir)),
        })
    }

    fn scan(
        &self,
        fixture: &Fixture,
        force_full: bool,
    ) -> Result<(WatchReindexOutcome, BTreeSet<PathBuf>)> {
        let filter = ActiveSessionSourceFilter::with_recent_write_window_for_test(None);
        let outcome = reindex_paths_with_semantic_delta(
            &fixture.options,
            vec![fixture.root.path.clone()],
            &fixture.roots(),
            &self.state,
            &self.storage,
            &self.index,
            &fixture.index_path,
            force_full,
            None,
            &filter,
        )?;
        Ok((outcome, filter.take_deferred_sources()))
    }

    fn close(self) -> Result<()> {
        let Self { storage, index, .. } = self;
        drop(index);
        storage.into_inner().unwrap().close()
    }
}

fn age_source(path: &Path) -> Result<()> {
    File::options()
        .write(true)
        .open(path)?
        .set_modified(UNIX_EPOCH + Duration::from_secs(1_600_000_000))?;
    Ok(())
}

fn canonical_rows(handles: &Handles) -> Result<Vec<(i64, i64, i64, String)>> {
    Ok(handles.storage.lock().unwrap().raw().query_map_collect(
        "SELECT conversation_id, id, idx, content FROM messages ORDER BY id",
        &[],
        |row| {
            Ok((
                row.get_typed(0)?,
                row.get_typed(1)?,
                row.get_typed(2)?,
                row.get_typed(3)?,
            ))
        },
    )?)
}

fn search_ids(fixture: &Fixture, term: &str) -> Result<BTreeSet<String>> {
    let reader = crate::search::quill_bridge::open_cass_reader(&fixture.index_path)?;
    let parser = frankensearch::quill::query::CassQueryParser::new(
        frankensearch::quill::schema::CASS_SEMANTIC_SCHEMA,
    )?;
    let query = parser.parse(
        term,
        &frankensearch::quill::query::CassQueryFilters::default(),
    );
    let page = crate::search::quill_bridge::search_paginated(&reader, &query.query, 512, 0, true)?;
    let ids: BTreeSet<_> = page
        .hits
        .iter()
        .map(|hit| hit.document_id.clone())
        .collect();
    assert_eq!(ids.len(), page.hits.len(), "duplicate lexical identities");
    assert_eq!(page.total_count, Some(ids.len()), "truncated search proof");
    Ok(ids)
}

fn assert_publication_receipts(handles: &Handles, expected: usize) -> Result<()> {
    let entries = handles
        .storage
        .lock()
        .unwrap()
        .source_ingest_ledger_entries()?;
    assert_eq!(entries.len(), expected, "durable source receipt count");
    for observation in entries.values() {
        let observation: serde_json::Value = serde_json::from_str(observation)?;
        assert_eq!(observation["watch_published"], true, "{observation}");
    }
    Ok(())
}

fn published_state(fixture: &Fixture, handles: &Handles) -> Result<(Option<i64>, Vec<u8>)> {
    Ok((
        handles.storage.lock().unwrap().get_last_indexed_at()?,
        fs::read(fixture.index_path.join("MANIFEST"))?,
    ))
}

fn active_retry_and_restart(kind: ConnectorKind) -> Result<()> {
    const OLD_SOURCES: usize = 128;

    let fixture = Fixture::new(kind)?;
    for index in 0..OLD_SOURCES {
        fixture.source(
            &format!("history-{index:04}"),
            &format!("receiptbaseline oldsource{index:04}"),
        )?;
    }
    let held = fixture.source("held", "receiptdeferredneedle")?;
    let lease = OpenOptions::new().read(true).write(true).open(&held)?;
    lease.try_lock()?;
    assert!(source_file_has_active_advisory_lock(&held));
    let handles = Handles::open(&fixture)?;

    // A broad initial callback publishes the closed sources in bounded batches
    // while the locked sibling remains queued. It must not misclassify that
    // unadmitted sibling as failed publication of the entire closed history.
    let (initial, deferred) = handles.scan(&fixture, true)?;
    assert_eq!(initial.indexed, OLD_SOURCES);
    assert_eq!(initial.published_conversations, OLD_SOURCES);
    assert_eq!(deferred, BTreeSet::from([held.clone()]));
    assert!(load_watch_lexical_replay_debt(&handles.storage.lock().unwrap())?.is_empty());
    assert_publication_receipts(&handles, OLD_SOURCES)?;
    let old_rows = canonical_rows(&handles)?;
    assert_eq!(old_rows.len(), OLD_SOURCES);
    let old_ids = search_ids(&fixture, "receiptbaseline")?;
    assert_eq!(old_ids.len(), OLD_SOURCES);
    assert!(search_ids(&fixture, "receiptdeferredneedle")?.is_empty());
    let published = published_state(&fixture, &handles)?;

    for _ in 0..3 {
        let (outcome, deferred) = handles.scan(&fixture, false)?;
        assert_eq!(
            outcome.indexed, 0,
            "unchanged closed history must not replay"
        );
        assert_eq!(outcome.published_conversations, 0);
        assert_eq!(deferred, BTreeSet::from([held.clone()]));
        assert_eq!(published_state(&fixture, &handles)?, published);
        assert_eq!(canonical_rows(&handles)?, old_rows);
    }

    // A closed source arriving while another remains active must publish
    // promptly, without replaying old history or claiming the complete source
    // set is caught up. Its preserved old mtime also bypasses event timestamps.
    fixture.source("arrived", "receiptarrivalneedle")?;
    let (arrival, deferred) = handles.scan(&fixture, false)?;
    assert_eq!(arrival.indexed, 1);
    assert_eq!(arrival.published_conversations, 1);
    assert_eq!(deferred, BTreeSet::from([held.clone()]));
    assert_eq!(
        handles.storage.lock().unwrap().get_last_indexed_at()?,
        published.0
    );
    assert!(load_watch_lexical_replay_debt(&handles.storage.lock().unwrap())?.is_empty());
    assert_eq!(search_ids(&fixture, "receiptarrivalneedle")?.len(), 1);
    assert_eq!(search_ids(&fixture, "receiptbaseline")?, old_ids);
    assert_publication_receipts(&handles, OLD_SOURCES + 1)?;
    let admitted_rows = canonical_rows(&handles)?;
    assert_eq!(admitted_rows.len(), OLD_SOURCES + 1);
    assert_eq!(&admitted_rows[..OLD_SOURCES], old_rows.as_slice());
    let published = published_state(&fixture, &handles)?;

    // Simulate debt retained by an older process. The active sibling can keep
    // that debt alive, but already-published source receipts must still bound
    // the retry instead of rebuilding all history on every cooldown tick.
    persist_watch_lexical_replay_debt(
        &handles.storage.lock().unwrap(),
        &WatchLexicalReplayDebt::for_root(kind, &fixture.root),
    )?;
    let (outcome, deferred) = handles.scan(&fixture, false)?;
    assert_eq!(outcome.indexed, 0);
    assert_eq!(outcome.published_conversations, 0);
    assert_eq!(deferred, BTreeSet::from([held.clone()]));
    assert_eq!(published_state(&fixture, &handles)?, published);
    assert_eq!(canonical_rows(&handles)?, admitted_rows);

    lease.unlock()?;
    drop(lease);
    let (outcome, deferred) = handles.scan(&fixture, false)?;
    assert_eq!(outcome.indexed, 1, "only the formerly locked source is new");
    assert!(deferred.is_empty());
    assert_eq!(search_ids(&fixture, "receiptdeferredneedle")?.len(), 1);
    assert!(load_watch_lexical_replay_debt(&handles.storage.lock().unwrap())?.is_empty());
    assert_publication_receipts(&handles, OLD_SOURCES + 2)?;

    let changed = fixture.path("history-0000");
    fixture.append(&changed, "history-0000", "receiptappendneedle")?;
    let (outcome, _) = handles.scan(&fixture, false)?;
    assert_eq!(outcome.indexed, 1, "one append must not rescan all history");
    assert_eq!(search_ids(&fixture, "receiptappendneedle")?.len(), 1);
    assert_eq!(search_ids(&fixture, "receiptbaseline")?, old_ids);
    let before_restart = canonical_rows(&handles)?;
    assert_eq!(before_restart.len(), OLD_SOURCES + 3);
    assert_eq!(&before_restart[..OLD_SOURCES], old_rows.as_slice());
    handles.close()?;

    fixture.append(&changed, "history-0000", "receiptrestartappend")?;
    // Both payload and filesystem time predate the saved provider watermark.
    // Only current source observations can find this import after restart.
    fixture.source("backdated", "receiptbackdatedneedle")?;
    let handles = Handles::open(&fixture)?;
    let (outcome, deferred) = handles.scan(&fixture, false)?;
    assert_eq!(
        outcome.indexed, 2,
        "restart must select only changed/new sources"
    );
    assert!(deferred.is_empty());
    for needle in [
        "receiptarrivalneedle",
        "receiptdeferredneedle",
        "receiptappendneedle",
        "receiptrestartappend",
        "receiptbackdatedneedle",
    ] {
        assert_eq!(search_ids(&fixture, needle)?.len(), 1, "{needle}");
    }
    assert_eq!(search_ids(&fixture, "receiptbaseline")?, old_ids);
    let recovered = canonical_rows(&handles)?;
    assert_eq!(recovered.len(), OLD_SOURCES + 5);
    assert_eq!(
        &recovered[..before_restart.len()],
        before_restart.as_slice()
    );
    assert_publication_receipts(&handles, OLD_SOURCES + 3)?;
    let published = published_state(&fixture, &handles)?;
    assert_eq!(handles.scan(&fixture, false)?.0.indexed, 0);
    assert_eq!(published_state(&fixture, &handles)?, published);
    assert_eq!(canonical_rows(&handles)?, recovered);
    handles.close()?;
    Ok(())
}

#[test]
#[serial]
fn gh538_watch_source_receipts_bound_codex_active_retries_and_restart() -> Result<()> {
    active_retry_and_restart(ConnectorKind::Codex)
}

#[test]
#[serial]
fn gh538_watch_source_receipts_bound_claude_active_retries_and_restart() -> Result<()> {
    active_retry_and_restart(ConnectorKind::Claude)
}

#[cfg(unix)]
#[test]
#[serial]
fn gh538_watch_discovery_failure_retries_without_inventing_publication_debt() -> Result<()> {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new(ConnectorKind::Codex)?;
    let source = fixture.source("complete", "discoverypreservedneedle")?;
    let handles = Handles::open(&fixture)?;
    assert_eq!(handles.scan(&fixture, true)?.0.indexed, 1);
    assert_publication_receipts(&handles, 1)?;
    let canonical = canonical_rows(&handles)?;
    let ids = search_ids(&fixture, "discoverypreservedneedle")?;
    assert_eq!(canonical.len(), 1);
    assert_eq!(ids.len(), 1);
    let published = published_state(&fixture, &handles)?;
    let source_bytes = fs::read(&source)?;
    let source_mtime = fs::metadata(&source)?.modified()?;

    // Codex probes root/sessions before walking a selected directory. A real
    // self-referential symlink makes discovery fail with ELOOP even when the
    // test runs as root; permission-bit fixtures cannot guarantee that error.
    // No source is admitted and no canonical transaction needs publication.
    let blocked_discovery = fixture.root.path.join("sessions");
    symlink("sessions", &blocked_discovery)?;
    assert!(fs::metadata(&blocked_discovery).is_err());
    let mut pending = BTreeSet::from([fixture.root.path.clone()]);
    let callback = |paths: Vec<PathBuf>, roots: &[(ConnectorKind, ScanRoot)], force_full| {
        assert_eq!(paths, vec![fixture.root.path.clone()]);
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].0, fixture.kind);
        assert_eq!(roots[0].1.path, fixture.root.path);
        assert!(!force_full);
        let result = handles.scan(&fixture, false);
        if let Err(error) = &result {
            assert!(error.downcast_ref::<ConnectorScanFailure>().is_some());
            assert!(
                format!("{error:#}").contains("Codex source discovery incomplete"),
                "the fixture must exercise discovery, not an unrelated failure: {error:#}"
            );
        }
        let (outcome, deferred) = result?;
        assert_eq!(outcome.indexed, 0, "published sources should be reused");
        assert_eq!(outcome.published_conversations, 0);
        assert!(deferred.is_empty());
        Ok(outcome.retry_sources)
    };
    assert!(!dispatch_watch_callback(
        &mut pending,
        &fixture.roots(),
        false,
        &callback,
    ));
    assert_eq!(pending, BTreeSet::from([fixture.root.path.clone()]));
    let debt = load_watch_lexical_replay_debt(&handles.storage.lock().unwrap())?;
    assert_eq!(
        debt.len(),
        1,
        "failed discovery must remain durably pending"
    );
    assert!(
        debt.iter().all(|debt| debt.discovery_only),
        "discovery failure must not invent publication debt for committed canonical rows"
    );
    assert_publication_receipts(&handles, 1)?;
    assert_eq!(canonical_rows(&handles)?, canonical);
    assert_eq!(published_state(&fixture, &handles)?, published);
    assert_eq!(search_ids(&fixture, "discoverypreservedneedle")?, ids);

    // Retain the failed filesystem object outside discovery, then retry the
    // same pending selection without another notification or source write.
    // Every source now reuses its receipt, so discovery-only debt must clear
    // without a publication batch or another lexical generation.
    fs::rename(
        &blocked_discovery,
        fixture._temp.path().join("parked-discovery-loop"),
    )?;
    assert!(dispatch_watch_callback(
        &mut pending,
        &fixture.roots(),
        false,
        &callback,
    ));
    assert!(pending.is_empty());
    assert!(
        watch_lexical_replay_sources(&handles.storage.lock().unwrap(), &fixture.roots())?
            .is_empty()
    );
    assert_publication_receipts(&handles, 1)?;
    assert_eq!(canonical_rows(&handles)?, canonical);
    assert_eq!(published_state(&fixture, &handles)?, published);
    assert_eq!(search_ids(&fixture, "discoverypreservedneedle")?, ids);
    assert_eq!(fs::read(&source)?, source_bytes);
    assert_eq!(fs::metadata(&source)?.modified()?, source_mtime);
    handles.close()?;
    Ok(())
}

#[test]
#[serial]
fn gh538_watch_recovery_reuses_locked_published_sibling_without_stranding_debt() -> Result<()> {
    for kind in [ConnectorKind::Codex, ConnectorKind::Claude] {
        let fixture = Fixture::new(kind)?;
        let unchanged = fixture.source("unchanged", "lockedpublishedneedle")?;
        let changed = fixture.source("changed", "changedpublishedneedle")?;
        let handles = Handles::open(&fixture)?;
        assert_eq!(handles.scan(&fixture, true)?.0.indexed, 2);
        assert_publication_receipts(&handles, 2)?;
        let old_rows = canonical_rows(&handles)?;
        let unchanged_ids = search_ids(&fixture, "lockedpublishedneedle")?;
        let changed_ids = search_ids(&fixture, "changedpublishedneedle")?;
        assert_eq!(old_rows.len(), 2);
        assert_eq!(unchanged_ids.len(), 1);
        assert_eq!(changed_ids.len(), 1);
        let published = published_state(&fixture, &handles)?;

        // The active sibling was already published and has not changed. A
        // failed append to the other source must not turn that valid receipt
        // into an obligation to replay or republish the locked sibling.
        let lease = OpenOptions::new().read(true).write(true).open(&unchanged)?;
        lease.try_lock()?;
        assert!(source_file_has_active_advisory_lock(&unchanged));
        fixture.append(&changed, "changed", "changedrecoveredneedle")?;
        {
            let _fault = FaultGuard::set(WatchLexicalTestFault::Commit);
            let error = handles
                .scan(&fixture, false)
                .err()
                .expect("the injected publication commit must fail");
            assert!(error.to_string().contains("injected watch lexical"));
        }
        let committed = canonical_rows(&handles)?;
        assert_eq!(committed.len(), 3);
        assert_eq!(&committed[..old_rows.len()], old_rows.as_slice());
        assert_eq!(published_state(&fixture, &handles)?, published);
        assert!(search_ids(&fixture, "changedrecoveredneedle")?.is_empty());
        assert_publication_receipts(&handles, 1)?;
        assert!(
            load_watch_lexical_replay_debt(&handles.storage.lock().unwrap())?
                .iter()
                .any(|debt| !debt.discovery_only),
            "the failed canonical append needs real publication recovery"
        );

        let (recovered, _) = handles.scan(&fixture, false)?;
        assert_eq!(recovered.indexed, 1, "only the changed source must replay");
        assert_eq!(recovered.published_conversations, 1);
        assert!(source_file_has_active_advisory_lock(&unchanged));
        assert_publication_receipts(&handles, 2)?;
        assert_eq!(canonical_rows(&handles)?, committed);
        assert_eq!(search_ids(&fixture, "changedrecoveredneedle")?.len(), 1);
        assert_eq!(
            search_ids(&fixture, "lockedpublishedneedle")?,
            unchanged_ids
        );
        assert_eq!(search_ids(&fixture, "changedpublishedneedle")?, changed_ids);

        lease.unlock()?;
        drop(lease);
        let published = published_state(&fixture, &handles)?;
        let (settled, deferred) = handles.scan(&fixture, false)?;
        assert_eq!(settled.indexed, 0, "all source receipts are complete");
        assert_eq!(settled.published_conversations, 0);
        assert!(deferred.is_empty());
        assert!(settled.retry_sources.is_empty());
        assert!(
            load_watch_lexical_replay_debt(&handles.storage.lock().unwrap())?.is_empty(),
            "a successfully repaired source must not leave all-reused root debt stranded"
        );
        assert_eq!(published_state(&fixture, &handles)?, published);
        assert_eq!(canonical_rows(&handles)?, committed);
        assert_eq!(search_ids(&fixture, "changedrecoveredneedle")?.len(), 1);
        assert_eq!(
            search_ids(&fixture, "lockedpublishedneedle")?,
            unchanged_ids
        );
        assert_eq!(search_ids(&fixture, "changedpublishedneedle")?, changed_ids);
        handles.close()?;
    }
    Ok(())
}

struct FaultGuard(Option<WatchLexicalTestFault>);

impl FaultGuard {
    fn set(fault: WatchLexicalTestFault) -> Self {
        Self(WATCH_LEXICAL_TEST_FAULT.with(|slot| slot.replace(Some(fault))))
    }
}

impl Drop for FaultGuard {
    fn drop(&mut self) {
        WATCH_LEXICAL_TEST_FAULT.with(|slot| slot.set(self.0));
    }
}

#[test]
#[serial]
fn gh538_watch_source_receipts_require_publication_before_reuse() -> Result<()> {
    for kind in [ConnectorKind::Codex, ConnectorKind::Claude] {
        for fault in [
            WatchLexicalTestFault::PublicationAfter(0),
            WatchLexicalTestFault::Commit,
            WatchLexicalTestFault::DebtClear,
        ] {
            let fixture = Fixture::new(kind)?;
            let changed = fixture.source("changed", "publishedbaselineneedle")?;
            fixture.source("untouched", "publishedsiblingneedle")?;
            let handles = Handles::open(&fixture)?;
            assert_eq!(handles.scan(&fixture, true)?.0.indexed, 2);
            assert_publication_receipts(&handles, 2)?;
            let old_ids = search_ids(&fixture, "publishedbaselineneedle")?;
            let sibling_ids = search_ids(&fixture, "publishedsiblingneedle")?;
            let published = published_state(&fixture, &handles)?;

            fixture.append(&changed, "changed", "publicationdeferredneedle")?;
            {
                let _fault = FaultGuard::set(fault);
                let result = handles.scan(&fixture, false);
                if fault != WatchLexicalTestFault::PublicationAfter(0) {
                    assert!(result.is_err(), "the injected persistence step must fail");
                } else {
                    let (outcome, _) = result?;
                    assert_eq!(outcome.indexed, 1);
                    assert_eq!(
                        outcome.retry_sources,
                        BTreeSet::from([fixture.root.path.clone()])
                    );
                }
            }
            let committed = canonical_rows(&handles)?;
            assert_eq!(
                committed.len(),
                3,
                "canonical append survives failed publication"
            );
            let failed_state = published_state(&fixture, &handles)?;
            assert_eq!(
                failed_state.0, published.0,
                "unfinished publication stays behind"
            );
            if fault == WatchLexicalTestFault::DebtClear {
                // Quill committed before final receipt/debt metadata failed.
                // The source must remain eligible for recovery, even though
                // its new content is already searchable in this generation.
                assert_ne!(failed_state.1, published.1);
                assert_eq!(search_ids(&fixture, "publicationdeferredneedle")?.len(), 1);
            } else {
                assert_eq!(failed_state.1, published.1);
                assert!(search_ids(&fixture, "publicationdeferredneedle")?.is_empty());
            }
            assert_eq!(search_ids(&fixture, "publishedbaselineneedle")?, old_ids);
            assert_eq!(search_ids(&fixture, "publishedsiblingneedle")?, sibling_ids);
            assert_publication_receipts(&handles, 1)?;
            assert!(!load_watch_lexical_replay_debt(&handles.storage.lock().unwrap())?.is_empty());
            handles.close()?;

            // Recover from durable debt with no source write or new event.
            // A still-valid sibling receipt must be reused even though the
            // changed source requires canonical replay after process restart.
            let handles = Handles::open(&fixture)?;
            assert_eq!(
                watch_lexical_replay_sources(&handles.storage.lock().unwrap(), &fixture.roots())?,
                BTreeSet::from([fixture.root.path.clone()]),
            );
            let (outcome, deferred) = handles.scan(&fixture, false)?;
            assert_eq!(
                outcome.indexed, 1,
                "only the unpublished source must replay"
            );
            assert!(deferred.is_empty());
            assert!(outcome.retry_sources.is_empty());
            assert_eq!(canonical_rows(&handles)?, committed);
            assert_eq!(search_ids(&fixture, "publicationdeferredneedle")?.len(), 1);
            assert_eq!(search_ids(&fixture, "publishedbaselineneedle")?, old_ids);
            assert_eq!(search_ids(&fixture, "publishedsiblingneedle")?, sibling_ids);
            assert_publication_receipts(&handles, 2)?;
            assert!(load_watch_lexical_replay_debt(&handles.storage.lock().unwrap())?.is_empty());

            // Explicit forced rebuilds retain their meaning even when all
            // source observations already have publication receipts.
            assert_eq!(handles.scan(&fixture, true)?.0.indexed, 2);
            assert_eq!(canonical_rows(&handles)?, committed);
            handles.close()?;
        }
    }
    Ok(())
}
