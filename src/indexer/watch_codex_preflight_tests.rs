//! Known canonical conflicts are isolated before watch publication. These tests
//! use the real Codex reader, source ledger, canonical writer, lexical reader,
//! and canonical semantic maintenance; late races still use the terminal path.

use super::*;
use crate::storage::sqlite::{CodexCanonicalRefusal, CodexCanonicalRefusalReason, SourceFileStamp};
use serial_test::serial;
use tempfile::TempDir;

struct Environment(Vec<(&'static str, Option<std::ffi::OsString>)>);

impl Environment {
    fn set(values: &[(&'static str, &str)]) -> Self {
        let previous = values
            .iter()
            .map(|(name, value)| {
                let previous = std::env::var_os(name);
                // Every test changing process policy uses the repository's
                // serial test lock, and restores its previous environment.
                unsafe { std::env::set_var(name, value) };
                (*name, previous)
            })
            .collect();
        Self(previous)
    }

    fn watch(chunk: &'static str) -> Self {
        Self::set(&[
            ("CASS_EXCLUDE_WORKSPACES", ""),
            ("CASS_WATCH_INGEST_CHUNK_SIZE", chunk),
            ("CASS_RAW_MIRROR", "1"),
            ("CASS_RAW_MIRROR_MIN_FREE_BYTES", "0"),
            ("CASS_RAW_MIRROR_MAX_BYTES", "67108864"),
        ])
    }
}

impl Drop for Environment {
    fn drop(&mut self) {
        for (name, previous) in self.0.drain(..).rev() {
            unsafe {
                match previous {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }
}

struct Fixture {
    _temp: TempDir,
    opts: IndexOptions,
    root: ScanRoot,
    paths: Vec<PathBuf>,
    source_bytes: Vec<Vec<u8>>,
    packets: Vec<NormalizedConversation>,
    keys: HashMap<PathBuf, String>,
    storage: Arc<Mutex<FrankenStorage>>,
    index: Mutex<Option<TantivyIndex>>,
    index_path: PathBuf,
    state: Mutex<HashMap<ConnectorKind, i64>>,
}

impl Fixture {
    fn new(remote: bool, sources: usize, messages: usize) -> Result<Self> {
        let temp = TempDir::new()?;
        let data_dir = temp.path().join("data");
        let provider_dir = temp.path().join("sessions");
        fs::create_dir_all(&data_dir)?;
        fs::create_dir_all(&provider_dir)?;
        let mut paths = Vec::new();
        let mut source_bytes = Vec::new();
        for source in 0..sources {
            let path = provider_dir.join(format!("rollout-preflight-{source:02}.jsonl"));
            let mut records = vec![serde_json::json!({
                "type": "session_meta",
                "payload": {
                    "id": format!("watch-preflight-{source:02}"),
                    "cwd": "/workspace/watch-preflight"
                }
            })];
            for message in 0..messages {
                records.push(serde_json::json!({
                    "type": "response_item",
                    "timestamp": format!("2026-11-23T12:{source:02}:{message:02}Z"),
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": format!(
                            "Explain the stable prefix preflightmarker{source}message{message} state machine"
                        )
                    }
                }));
            }
            let mut bytes = Vec::new();
            for record in records {
                serde_json::to_writer(&mut bytes, &record)?;
                bytes.push(b'\n');
            }
            fs::write(&path, &bytes)?;
            File::open(&path)?.set_modified(SystemTime::now() - Duration::from_secs(7200))?;
            paths.push(path);
            source_bytes.push(bytes);
        }
        let root = if remote {
            ScanRoot::remote(
                provider_dir,
                Origin::remote("watch-preflight-fixture"),
                None,
            )
        } else {
            ScanRoot::local(provider_dir)
        };
        let ctx = ScanContext::with_roots(root.path.clone(), vec![root.clone()], None);
        let connector = CodexConnector::new();
        let keys = connector
            .discover_source_files(&ctx)?
            .into_iter()
            .map(|source| {
                Ok((
                    source.source_path.clone(),
                    source_ledger_key(&source, &ctx).context("source key")?,
                ))
            })
            .collect::<Result<HashMap<_, _>>>()?;
        let mut ledger = watch_source_ledger::WatchSourceLedger::default();
        let mut packets = Vec::new();
        ledger.scan(
            &connector,
            "codex",
            &ctx,
            &HashMap::new(),
            false,
            false,
            &ActiveSessionSourceFilter::new(true),
            None,
            &mut |mut conversation| {
                prepare_conversation_metadata_for_ingest(
                    "codex",
                    &root.origin,
                    Some(&root),
                    &mut conversation,
                );
                packets.push(conversation);
                Ok(true)
            },
        )?;
        packets.sort_by_key(|packet| packet.source_path.clone());
        assert_eq!(packets.len(), sources);
        assert_eq!(ledger.completed_source_paths().count(), sources);
        for (packet, path) in packets.iter().zip(&paths) {
            assert_eq!(&packet.source_path, path);
            assert_eq!(packet.messages.len(), messages);
            assert!(
                packet
                    .metadata
                    .pointer("/cass/codex_stable_prefix")
                    .is_some()
            );
        }
        let db_path = data_dir.join("archive.db");
        let storage = Arc::new(Mutex::new(FrankenStorage::open(&db_path)?));
        let index_path = index_dir(&data_dir)?;
        let index = Mutex::new(Some(TantivyIndex::open_or_create(&index_path)?));
        let opts = IndexOptions {
            full: false,
            force_rebuild: false,
            watch: true,
            watch_once_paths: None,
            db_path,
            data_dir,
            semantic: true,
            build_hnsw: false,
            embedder: "hash".to_owned(),
            progress: Some(Arc::new(IndexingProgress::default())),
            watch_interval_secs: 30,
        };
        storage.lock().unwrap().set_last_indexed_at(1234)?;
        Ok(Self {
            _temp: temp,
            opts,
            root,
            paths,
            source_bytes,
            packets,
            keys,
            storage,
            index,
            index_path,
            state: Mutex::new(HashMap::from([(ConnectorKind::Codex, 1234)])),
        })
    }

    fn scan(
        &self,
    ) -> Result<(
        watch_source_ledger::WatchSourceLedger,
        Vec<NormalizedConversation>,
    )> {
        let ctx = ScanContext::with_roots(self.root.path.clone(), vec![self.root.clone()], None);
        let mut ledger = watch_source_ledger::WatchSourceLedger::default();
        let mut packets = Vec::new();
        ledger.scan(
            &CodexConnector::new(),
            "codex",
            &ctx,
            &HashMap::new(),
            false,
            false,
            &ActiveSessionSourceFilter::new(true),
            None,
            &mut |mut conversation| {
                prepare_conversation_metadata_for_ingest(
                    "codex",
                    &self.root.origin,
                    Some(&self.root),
                    &mut conversation,
                );
                packets.push(conversation);
                Ok(true)
            },
        )?;
        packets.sort_by_key(|packet| packet.source_path.clone());
        Ok((ledger, packets))
    }

    fn seed_conflict(&self, source: usize) -> Result<NormalizedConversation> {
        let mut legacy = without_prefix(self.packets[source].clone());
        legacy.messages.truncate(1);
        legacy.messages[0].content =
            format!("Retained canonical history for source {source} must remain unchanged");
        legacy.ended_at = legacy.messages[0].created_at;
        let storage = self.storage.lock().unwrap();
        let mut index = self.index.lock().unwrap();
        let index = index.as_mut().context("seed lexical writer")?;
        persist::persist_conversation(&storage, index, &legacy)?;
        index.commit()?;
        Ok(legacy)
    }

    fn run(&self, explicit: bool) -> Result<WatchReindexOutcome> {
        let mut opts = self.opts.clone();
        if explicit {
            opts.watch = false;
            opts.watch_once_paths = Some(vec![self.root.path.clone()]);
        }
        reindex_paths_with_semantic_delta(
            &opts,
            vec![self.root.path.clone()],
            &[(ConnectorKind::Codex, self.root.clone())],
            &self.state,
            &self.storage,
            &self.index,
            &self.index_path,
            false,
            None,
            &ActiveSessionSourceFilter::new(!explicit),
        )
    }

    fn receipt(&self, source: usize) -> Result<serde_json::Value> {
        let entries = self
            .storage
            .lock()
            .unwrap()
            .source_ingest_ledger_entries()?;
        serde_json::from_str(
            entries
                .get(&self.keys[&self.paths[source]])
                .context("retained source proof")?,
        )
        .context("source receipt JSON")
    }

    fn assert_retry_debt(&self) -> Result<()> {
        let storage = self.storage.lock().unwrap();
        let debts = watch_lexical_replay_debt_for_root(&storage, ConnectorKind::Codex, &self.root)?;
        assert!(!debts.is_empty());
        assert!(debts.iter().any(|debt| !debt.discovery_only));
        let index = self.index.lock().unwrap();
        assert!(
            !refresh_completed_watch_lexical_checkpoint(
                &storage,
                index.as_ref().context("retained lexical writer")?,
                &self.opts.db_path,
                &self.index_path,
                true,
            )?,
            "unresolved canonical source must prevent a global lexical certificate"
        );
        Ok(())
    }

    fn assert_sources_unchanged(&self) -> Result<()> {
        for (path, bytes) in self.paths.iter().zip(&self.source_bytes) {
            assert_eq!(&fs::read(path)?, bytes);
        }
        Ok(())
    }
}

fn without_prefix(mut packet: NormalizedConversation) -> NormalizedConversation {
    packet.metadata["cass"]
        .as_object_mut()
        .unwrap()
        .remove("codex_stable_prefix");
    packet
}

type MessageSnapshot = (
    i64,
    i64,
    String,
    Option<String>,
    Option<i64>,
    String,
    Option<String>,
    Option<Vec<u8>>,
);
type ConversationSnapshot = (
    i64,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<String>,
    Option<Vec<u8>>,
);
type SourceSnapshot = (Vec<MessageSnapshot>, Vec<ConversationSnapshot>);

fn source_snapshot(storage: &FrankenStorage, path: &Path) -> Result<SourceSnapshot> {
    let path = path.to_string_lossy();
    let messages = storage.raw().query_map_collect(
        "SELECT m.id, m.idx, m.role, m.author, m.created_at, m.content, m.extra_json, m.extra_bin
         FROM messages m JOIN conversations c ON c.id = m.conversation_id
         WHERE c.source_path = ?1 ORDER BY m.id",
        &[ParamValue::from(path.as_ref())],
        |row| {
            Ok((
                row.get_typed(0)?,
                row.get_typed(1)?,
                row.get_typed(2)?,
                row.get_typed(3)?,
                row.get_typed(4)?,
                row.get_typed(5)?,
                row.get_typed(6)?,
                row.get_typed(7)?,
            ))
        },
    )?;
    let conversations = storage.raw().query_map_collect(
        "SELECT id, title, started_at, ended_at, metadata_json, metadata_bin
         FROM conversations WHERE source_path = ?1 ORDER BY id",
        &[ParamValue::from(path.as_ref())],
        |row| {
            Ok((
                row.get_typed(0)?,
                row.get_typed(1)?,
                row.get_typed(2)?,
                row.get_typed(3)?,
                row.get_typed(4)?,
                row.get_typed(5)?,
            ))
        },
    )?;
    Ok((messages, conversations))
}

fn search_ids(index_path: &Path, term: &str) -> Result<BTreeSet<String>> {
    let reader = crate::search::quill_bridge::open_cass_reader(index_path)?;
    let parser = frankensearch::quill::query::CassQueryParser::new(
        frankensearch::quill::schema::CASS_SEMANTIC_SCHEMA,
    )?;
    let query = parser.parse(
        term,
        &frankensearch::quill::query::CassQueryFilters::default(),
    );
    let page = crate::search::quill_bridge::search_paginated(&reader, &query.query, 32, 0, true)?;
    let ids: BTreeSet<_> = page
        .hits
        .iter()
        .map(|hit| hit.document_id.clone())
        .collect();
    assert_eq!(
        ids.len(),
        page.hits.len(),
        "lexical identities must be unique"
    );
    assert_eq!(page.total_count, Some(ids.len()));
    Ok(ids)
}

fn raw_paths(data_dir: &Path) -> Result<BTreeSet<PathBuf>> {
    let directory = data_dir.join("raw-mirror/v1/manifests");
    if !directory.exists() {
        return Ok(BTreeSet::new());
    }
    fs::read_dir(directory)?
        .map(|entry| {
            let manifest: serde_json::Value = serde_json::from_slice(&fs::read(entry?.path())?)?;
            Ok(PathBuf::from(
                manifest["original_path"]
                    .as_str()
                    .context("raw source path")?,
            ))
        })
        .collect()
}

#[test]
#[serial]
fn watch_codex_preflight_local_and_remote_bad_first_middle_last_publish_healthy_sources()
-> Result<()> {
    let _environment = Environment::watch("32");
    for remote in [false, true] {
        for refused in 0..3 {
            let fixture = Fixture::new(remote, 3, 2)?;
            fixture.seed_conflict(refused)?;
            let before =
                source_snapshot(&fixture.storage.lock().unwrap(), &fixture.paths[refused])?;
            run_targeted_semantic_watch_once_publish(
                "hash",
                &fixture.opts.data_dir,
                &fixture.storage,
                1,
                0,
            )?;
            let mut maintenance = WatchSemanticMaintenance::new(1);
            let outcome = fixture.run(false)?;
            assert_eq!(outcome.indexed, 2, "remote={remote}, refused={refused}");
            assert_eq!(outcome.published_conversations, 2);
            assert!(outcome.retry_sources.contains(&fixture.root.path));
            assert_eq!(
                source_snapshot(&fixture.storage.lock().unwrap(), &fixture.paths[refused])?,
                before,
                "preflight must not write any part of the refused source"
            );
            assert_eq!(
                count_total_conversations_exact(&fixture.storage.lock().unwrap())?,
                3
            );
            assert_eq!(
                count_total_messages_exact(&fixture.storage.lock().unwrap())?,
                5
            );
            let raw = raw_paths(&fixture.opts.data_dir)?;
            assert!(!raw.contains(&fixture.paths[refused]));
            for source in 0..3 {
                let receipt = fixture.receipt(source)?;
                if source == refused {
                    assert_eq!(receipt["publication_invalidated"], true);
                    assert_eq!(receipt["watch_published"], false);
                    assert!(
                        search_ids(
                            &fixture.index_path,
                            &format!("preflightmarker{source}message1")
                        )?
                        .is_empty()
                    );
                } else {
                    assert_eq!(receipt["watch_published"], true);
                    assert!(receipt.get("publication_invalidated").is_none());
                    assert!(raw.contains(&fixture.paths[source]));
                    for message in 0..2 {
                        assert_eq!(
                            search_ids(
                                &fixture.index_path,
                                &format!("preflightmarker{source}message{message}")
                            )?
                            .len(),
                            1
                        );
                    }
                }
            }
            assert_eq!(
                fixture.state.lock().unwrap().get(&ConnectorKind::Codex),
                Some(&1234)
            );
            assert_eq!(
                fixture.storage.lock().unwrap().get_last_indexed_at()?,
                Some(1234)
            );
            let progress = fixture.opts.progress.as_ref().unwrap();
            let stats = progress.stats.lock().unwrap();
            assert!(stats.scan_had_errors);
            assert!(stats.connectors.iter().any(|entry| {
                entry
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("canonical"))
            }));
            drop(stats);
            fixture.assert_retry_debt()?;
            // This is the live watch caller's real canonical maintenance path:
            // a diagnostic refusal must not discard healthy publication work.
            maintenance.record_ingest(outcome.published_conversations, fixture.paths.clone());
            let semantic = maintenance
                .reconcile_if_due(
                    "hash",
                    &fixture.opts.data_dir,
                    &fixture.storage,
                    Duration::ZERO,
                    None,
                )?
                .context("healthy semantic publication")?;
            assert!(semantic.published);
            let canonical =
                CanonicalSemanticDocuments::from_storage(&fixture.storage.lock().unwrap())?;
            let vectors =
                FsVectorIndex::open(&vector_index_path(&fixture.opts.data_dir, "fnv1a-384"))?;
            let actual = (0..vectors.record_count())
                .map(|position| vectors.doc_id_at(position).map(str::to_owned))
                .collect::<Result<HashSet<_>, _>>()?;
            assert_eq!(actual, canonical.doc_ids);
            assert!(maintenance.pending_paths.is_empty());
            fixture.assert_sources_unchanged()?;
        }
    }
    Ok(())
}

#[test]
#[serial]
fn watch_codex_preflight_multiple_chunks_and_restart_preserve_healthy_receipts() -> Result<()> {
    let _environment = Environment::watch("1");
    for refused in [1, 2] {
        let fixture = Fixture::new(false, 3, 2)?;
        fixture.seed_conflict(refused)?;
        let outcome = fixture.run(false)?;
        assert_eq!(outcome.published_conversations, 2);
        assert!(outcome.retry_sources.contains(&fixture.root.path));
        let before = fixture
            .storage
            .lock()
            .unwrap()
            .source_ingest_ledger_entries()?;
        let rows = count_total_messages_exact(&fixture.storage.lock().unwrap())?;
        for source in 0..3 {
            assert_eq!(
                fixture.receipt(source)?["watch_published"],
                source != refused
            );
        }
        fixture.assert_retry_debt()?;
        // A new storage handle, index handle and watch state simulate daemon
        // restart; no new source event or append is needed for debt discovery.
        *fixture.index.lock().unwrap() = None;
        let storage = Mutex::new(FrankenStorage::open(&fixture.opts.db_path)?);
        let index = Mutex::new(None);
        let roots = [(ConnectorKind::Codex, fixture.root.clone())];
        let retry = watch_lexical_replay_sources(&storage.lock().unwrap(), &roots)?;
        assert!(retry.contains(&fixture.root.path));
        let outcome = reindex_paths_with_semantic_delta(
            &fixture.opts,
            retry.into_iter().collect(),
            &roots,
            &Mutex::new(HashMap::new()),
            &storage,
            &index,
            &fixture.index_path,
            false,
            None,
            &ActiveSessionSourceFilter::new(true),
        )?;
        assert_eq!(
            outcome.published_conversations, 0,
            "complete healthy receipts must be reused"
        );
        assert!(outcome.retry_sources.contains(&fixture.root.path));
        assert_eq!(count_total_messages_exact(&storage.lock().unwrap())?, rows);
        assert_eq!(
            storage.lock().unwrap().source_ingest_ledger_entries()?,
            before
        );
        assert!(!load_watch_lexical_replay_debt(&storage.lock().unwrap())?.is_empty());
        for source in (0..3).filter(|source| *source != refused) {
            assert_eq!(
                search_ids(
                    &fixture.index_path,
                    &format!("preflightmarker{source}message1")
                )?
                .len(),
                1
            );
        }
        fixture.assert_sources_unchanged()?;
    }
    Ok(())
}

#[test]
#[serial]
fn watch_codex_preflight_all_refused_and_explicit_selection_remain_incomplete() -> Result<()> {
    let _environment = Environment::watch("32");
    let all = Fixture::new(false, 2, 2)?;
    for source in 0..2 {
        all.seed_conflict(source)?;
    }
    let outcome = all.run(false)?;
    assert_eq!(outcome.indexed, 0);
    assert_eq!(outcome.published_conversations, 0);
    assert!(outcome.retry_sources.contains(&all.root.path));
    assert!(raw_paths(&all.opts.data_dir)?.is_empty());
    all.assert_retry_debt()?;
    for source in 0..2 {
        assert_eq!(all.receipt(source)?["publication_invalidated"], true);
    }
    let explicit = Fixture::new(true, 3, 2)?;
    explicit.seed_conflict(1)?;
    let error = explicit
        .run(true)
        .err()
        .context("incomplete one-shot selection succeeded")?;
    assert!(error.downcast_ref::<CodexCanonicalRefusal>().is_some());
    for source in [0, 2] {
        assert_eq!(explicit.receipt(source)?["watch_published"], true);
        assert_eq!(
            search_ids(
                &explicit.index_path,
                &format!("preflightmarker{source}message1")
            )?
            .len(),
            1
        );
    }
    assert_eq!(explicit.receipt(1)?["watch_published"], false);
    explicit.assert_retry_debt()?;
    Ok(())
}

#[test]
#[serial]
fn watch_codex_preflight_untimed_late_slice_is_bounded_and_writes_nothing() -> Result<()> {
    let _environment = Environment::watch("32");
    let fixture = Fixture::new(false, 1, 4)?;
    let mut packet = fixture.packets[0].clone();
    packet.started_at = None;
    packet.ended_at = None;
    for message in &mut packet.messages {
        message.created_at = None;
    }
    let mut legacy = without_prefix(packet.clone());
    legacy.messages = vec![legacy.messages[0].clone(), legacy.messages[3].clone()];
    legacy.messages[1].content = "Retained canonical conflict in the last untimed slice".to_owned();
    {
        let storage = fixture.storage.lock().unwrap();
        let mut index = fixture.index.lock().unwrap();
        persist::persist_conversation(&storage, index.as_mut().unwrap(), &legacy)?;
        index.as_mut().unwrap().commit()?;
    }
    let before = source_snapshot(&fixture.storage.lock().unwrap(), &fixture.paths[0])?;
    persist::CODEX_PREFLIGHT_PROJECTION_OBSERVATION.with(|observation| observation.set((0, 0)));
    let refusal = persist::preflight_codex_canonical_source(
        &fixture.storage.lock().unwrap(),
        &packet,
        persist::PersistHeartbeat::NONE,
        persist::PersistSliceLimits {
            messages: 2,
            content_bytes: usize::MAX,
        },
    )?
    .context("late untimed canonical conflict was not found")?;
    assert!(matches!(
        refusal.reason,
        CodexCanonicalRefusalReason::ProjectionMismatch(3)
    ));
    assert_eq!(
        persist::CODEX_PREFLIGHT_PROJECTION_OBSERVATION.with(|observation| observation.get()),
        (2, 2),
        "observe actual mapped allocations: missing timestamps must not bypass slicing"
    );
    assert_eq!(
        source_snapshot(&fixture.storage.lock().unwrap(), &fixture.paths[0])?,
        before
    );
    assert_eq!(
        count_total_messages_exact(&fixture.storage.lock().unwrap())?,
        2
    );
    assert!(
        fixture
            .storage
            .lock()
            .unwrap()
            .source_ingest_ledger_entries()?
            .is_empty()
    );
    assert!(raw_paths(&fixture.opts.data_dir)?.is_empty());
    Ok(())
}

#[test]
#[serial]
fn watch_codex_preflight_policy_exclusion_and_changed_tombstones_are_read_only() -> Result<()> {
    let _environment = Environment::watch("32");
    let fixture = Fixture::new(false, 1, 2)?;
    fixture.seed_conflict(0)?;
    let source = fixture.paths[0].to_string_lossy();
    let stamp = SourceFileStamp::of(&fixture.paths[0]);
    fixture.storage.lock().unwrap().raw().execute_compat(
        "INSERT INTO forgotten_sources(source_path, size_bytes, mtime_ms, forgotten_at_ms) VALUES(?1, ?2, ?3, 1)",
        &[ParamValue::from(source.as_ref()), ParamValue::from(stamp.size_bytes.unwrap()), ParamValue::from(stamp.mtime_ms.unwrap())],
    )?;
    let before = fixture.storage.lock().unwrap().forgotten_source_stamps()?;
    let (mut ledger, mut packets) = fixture.scan()?;
    let preflight = preflight_watch_codex_sources(
        &fixture.storage.lock().unwrap(),
        &mut ledger,
        &mut packets,
        &None,
    )?;
    assert!(
        preflight.first_refusal.is_none(),
        "a deliberately forgotten source is not a canonical failure"
    );
    assert_eq!(preflight.capture_allowed, vec![false]);
    assert!(!ledger.has_canonical_refusals());
    assert_eq!(
        fixture.storage.lock().unwrap().forgotten_source_stamps()?,
        before
    );
    fixture.storage.lock().unwrap().raw().execute_compat(
        "UPDATE forgotten_sources SET size_bytes = 0 WHERE source_path = ?1",
        &[ParamValue::from(source.as_ref())],
    )?;
    let changed = fixture.storage.lock().unwrap().forgotten_source_stamps()?;
    let (mut ledger, mut packets) = fixture.scan()?;
    let preflight = preflight_watch_codex_sources(
        &fixture.storage.lock().unwrap(),
        &mut ledger,
        &mut packets,
        &None,
    )?;
    assert!(preflight.first_refusal.is_some());
    assert!(packets.is_empty());
    assert_eq!(
        fixture.storage.lock().unwrap().forgotten_source_stamps()?,
        changed,
        "preflight must not clear a changed stamp before any accepted writer runs"
    );
    {
        let _policy =
            Environment::set(&[("CASS_EXCLUDE_WORKSPACES", "/workspace/watch-preflight")]);
        let outcome = fixture.run(false)?;
        assert!(
            outcome.retry_sources.is_empty(),
            "intentional workspace exclusion must not become a canonical retry loop"
        );
        assert_eq!(
            count_total_messages_exact(&fixture.storage.lock().unwrap())?,
            1
        );
        assert_eq!(
            fixture.storage.lock().unwrap().forgotten_source_stamps()?,
            changed
        );
        assert!(raw_paths(&fixture.opts.data_dir)?.is_empty());
        assert_eq!(
            fixture.receipt(0)?["watch_published"],
            true,
            "policy-bound completion remains valid"
        );
    }
    fixture.assert_sources_unchanged()?;
    Ok(())
}

#[test]
#[serial]
fn watch_codex_preflight_refuses_all_same_source_packets_and_later_chunks() -> Result<()> {
    let _environment = Environment::watch("32");
    let fixture = Fixture::new(false, 2, 2)?;
    fixture.seed_conflict(0)?;
    let (mut ledger, mut packets) = fixture.scan()?;
    let healthy = packets.pop().context("healthy later-chunk source")?;
    let mut initially_clean = packets[0].clone();
    initially_clean.external_id = Some("same-source-other-packet".to_owned());
    packets.insert(0, initially_clean.clone());
    let original = packets.clone();
    let debt = WatchLexicalReplayDebt::for_root(ConnectorKind::Codex, &fixture.root);
    ledger.begin_publication(&fixture.storage.lock().unwrap(), &debt, None)?;
    let before = fixture
        .storage
        .lock()
        .unwrap()
        .source_ingest_ledger_entries()?;
    let result = preflight_watch_codex_sources(
        &fixture.storage.lock().unwrap(),
        &mut ledger,
        &mut packets,
        &None,
    )?;
    assert_eq!(result.deferred_conversations, 2);
    assert!(result.first_refusal.is_some());
    assert!(
        packets.is_empty(),
        "a later refusal must remove an earlier clean packet of that source"
    );
    assert!(!ledger.may_publish_source(&fixture.paths[0]));
    ledger.publish_batch(&fixture.storage.lock().unwrap(), &original, &[], false)?;
    assert_eq!(
        fixture
            .storage
            .lock()
            .unwrap()
            .source_ingest_ledger_entries()?,
        before
    );
    // The helper includes the real provider-aware alignment stage. Repeating
    // the refused packet must not turn its deliberately uncertified completion
    // into a terminal missing-proof error ahead of this healthy sibling.
    let mut later = vec![initially_clean, healthy];
    let result = preflight_watch_codex_sources(
        &fixture.storage.lock().unwrap(),
        &mut ledger,
        &mut later,
        &None,
    )?;
    assert_eq!(result.deferred_conversations, 1);
    assert_eq!(later.len(), 1);
    assert_eq!(later[0].source_path, fixture.paths[1]);
    assert_eq!(
        count_total_messages_exact(&fixture.storage.lock().unwrap())?,
        1
    );
    fixture.assert_retry_debt()?;
    Ok(())
}

#[test]
#[serial]
fn watch_codex_preflight_preserves_strict_provider_and_source_alignment() -> Result<()> {
    let _environment = Environment::watch("32");
    let fixture = Fixture::new(false, 1, 2)?;
    let mut missing = watch_source_ledger::WatchSourceLedger::default();
    let error = preflight_watch_codex_sources(
        &fixture.storage.lock().unwrap(),
        &mut missing,
        &mut vec![fixture.packets[0].clone()],
        &None,
    )
    .err()
    .context("an admitted prefix without certified completion was accepted")?;
    assert!(error.downcast_ref::<CodexCanonicalRefusal>().is_none());
    for foreign_provider in [false, true] {
        let (mut ledger, mut packets) = fixture.scan()?;
        if foreign_provider {
            packets[0].agent_slug = "claude_code".to_owned();
        } else {
            packets[0].metadata["cass"]["codex_stable_prefix"]["source_path"] =
                serde_json::to_value(fixture.root.path.join("foreign.jsonl"))?;
        }
        let error = preflight_watch_codex_sources(
            &fixture.storage.lock().unwrap(),
            &mut ledger,
            &mut packets,
            &None,
        )
        .err()
        .context("foreign provider/source proof was accepted")?;
        assert!(error.downcast_ref::<CodexCanonicalRefusal>().is_none());
        assert!(!ledger.has_canonical_refusals());
    }
    // Remembered refusal eligibility applies to the source even when a later
    // packet carries a different provider label. The non-Codex fast path must
    // filter it instead of bypassing both proof alignment and final retention.
    let mut refused = watch_source_ledger::WatchSourceLedger::default();
    refused.defer_canonical_source(&fixture.paths[0])?;
    let mut later = vec![fixture.packets[0].clone()];
    later[0].agent_slug = "claude_code".to_owned();
    let result = preflight_watch_codex_sources(
        &fixture.storage.lock().unwrap(),
        &mut refused,
        &mut later,
        &None,
    )?;
    assert_eq!(result.deferred_conversations, 1);
    assert!(result.capture_allowed.is_empty());
    assert!(later.is_empty());
    assert_eq!(
        count_total_conversations_exact(&fixture.storage.lock().unwrap())?,
        0
    );
    assert!(raw_paths(&fixture.opts.data_dir)?.is_empty());
    fixture.assert_sources_unchanged()?;
    Ok(())
}

#[test]
#[serial]
fn watch_codex_preflight_compares_the_same_redacted_projection_as_persistence() -> Result<()> {
    let _environment = Environment::watch("32");
    let _redaction = Environment::set(&[("CASS_INDEX_REDACTION", "full")]);
    let mut fixture = Fixture::new(false, 1, 2)?;
    let secret = "AKIAIOSFODNN7EXAMPLE";
    let bytes = fs::read_to_string(&fixture.paths[0])?
        .replace("preflightmarker0message0", secret)
        .into_bytes();
    fs::write(&fixture.paths[0], &bytes)?;
    File::open(&fixture.paths[0])?.set_modified(SystemTime::now() - Duration::from_secs(7200))?;
    fixture.source_bytes[0] = bytes;
    let (mut ledger, mut packets) = fixture.scan()?;
    assert!(packets[0].messages[0].content.contains(secret));
    assert!(packets[0].external_id.is_some());
    let legacy = without_prefix(packets[0].clone());
    {
        let storage = fixture.storage.lock().unwrap();
        let mut index = fixture.index.lock().unwrap();
        persist::persist_conversation(&storage, index.as_mut().unwrap(), &legacy)?;
        index.as_mut().unwrap().commit()?;
    }
    let before = source_snapshot(&fixture.storage.lock().unwrap(), &fixture.paths[0])?;
    assert!(
        before
            .0
            .iter()
            .all(|(_, _, _, _, _, content, _, _)| !content.contains(secret))
    );
    let result = preflight_watch_codex_sources(
        &fixture.storage.lock().unwrap(),
        &mut ledger,
        &mut packets,
        &None,
    )?;
    assert!(
        result.first_refusal.is_none(),
        "source plaintext must be redacted before comparing canonical rows"
    );
    assert_eq!(result.capture_allowed, vec![true]);
    assert_eq!(
        source_snapshot(&fixture.storage.lock().unwrap(), &fixture.paths[0])?,
        before
    );
    fixture.run(false)?;
    assert_eq!(
        count_total_messages_exact(&fixture.storage.lock().unwrap())?,
        2
    );
    assert_eq!(fixture.receipt(0)?["watch_published"], true);
    assert!(search_ids(&fixture.index_path, secret)?.is_empty());
    fixture.assert_sources_unchanged()?;
    Ok(())
}

struct ProbeRaceGuard;

impl Drop for ProbeRaceGuard {
    fn drop(&mut self) {
        WATCH_CODEX_PREFLIGHT_TEST_HOOK.with(|hook| {
            hook.borrow_mut().take();
        });
    }
}

#[test]
#[serial]
fn watch_codex_preflight_clean_probe_late_conflict_remains_terminal() -> Result<()> {
    let _environment = Environment::watch("32");
    let fixture = Fixture::new(false, 1, 2)?;
    let mut competing = without_prefix(fixture.packets[0].clone());
    competing.messages.truncate(1);
    competing.messages[0].content =
        "A competing canonical writer committed after the read-only probe".to_owned();
    let storage = fixture.storage.clone();
    let raced = std::rc::Rc::new(std::cell::Cell::new(false));
    let raced_in_hook = raced.clone();
    let _race = ProbeRaceGuard;
    WATCH_CODEX_PREFLIGHT_TEST_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            persist::persist_conversations_batched(
                &storage.lock().unwrap(),
                None,
                &[competing],
                LexicalPopulationStrategy::DeferredAuthoritativeDbRebuild,
                false,
            )
            .expect("competing canonical write");
            raced_in_hook.set(true);
        }));
    });
    let error = fixture
        .run(false)
        .err()
        .context("post-probe conflict was swallowed")?;
    assert!(raced.get(), "the race must occur after a clean preflight");
    assert!(error.downcast_ref::<CodexCanonicalRefusal>().is_some());
    assert_eq!(
        count_total_messages_exact(&fixture.storage.lock().unwrap())?,
        1
    );
    assert_eq!(
        fixture
            .index
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .doc_count()?,
        0
    );
    assert_eq!(fixture.receipt(0)?["publication_invalidated"], true);
    assert_eq!(fixture.receipt(0)?["watch_published"], false);
    fixture.assert_retry_debt()?;
    fixture.assert_sources_unchanged()?;
    Ok(())
}

#[test]
#[serial]
fn watch_codex_preflight_malformed_canonical_metadata_is_not_a_domain_deferral() -> Result<()> {
    let _environment = Environment::watch("32");
    let fixture = Fixture::new(false, 1, 2)?;
    fixture.seed_conflict(0)?;
    fixture
        .storage
        .lock()
        .unwrap()
        .raw()
        .execute_batch("UPDATE conversations SET metadata_bin = NULL, metadata_json = '{'")?;
    let error = fixture
        .run(false)
        .err()
        .context("malformed canonical metadata was swallowed")?;
    assert!(error.downcast_ref::<CodexCanonicalRefusal>().is_none());
    assert_eq!(
        count_total_messages_exact(&fixture.storage.lock().unwrap())?,
        1
    );
    assert_eq!(fixture.receipt(0)?["publication_invalidated"], true);
    assert!(raw_paths(&fixture.opts.data_dir)?.is_empty());
    fixture.assert_retry_debt()?;
    Ok(())
}
