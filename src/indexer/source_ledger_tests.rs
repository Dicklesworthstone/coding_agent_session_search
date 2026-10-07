//! GH512: use the production match and completion helpers, including a live
//! pre-fix comparator in the workload-shaped ignored benchmark.

use super::*;
use std::hint::black_box;
use std::sync::{Arc, Barrier};
use tempfile::TempDir;

fn discovered(path: &Path, slug: &str) -> DiscoveredSourceFile {
    DiscoveredSourceFile::new(
        slug,
        &ScanRoot::local(path.parent().unwrap().to_path_buf()),
        path.to_path_buf(),
        crate::connectors::DiscoveredSourceRole::PrimarySessionLog,
        true,
    )
    .with_fs_metadata()
}

fn saved_row(source: &DiscoveredSourceFile, dependencies: Vec<serde_json::Value>) -> String {
    serde_json::json!({
        "primary": source_file_observation(&source.source_path).unwrap(),
        "producer_contract": env!("CASS_SOURCE_INGEST_CONTRACT"),
        "dependencies": dependencies,
    })
    .to_string()
}

fn legacy_row(source: &DiscoveredSourceFile) -> String {
    saved_row(
        source,
        vec![source_file_observation(source.source_path.parent().unwrap()).unwrap()],
    )
}

/// No timestamp sleeps: require an observed directory change before testing
/// invalidation. Unique files also force directory growth on coarse clocks.
fn grow_directory(path: &Path, prefix: &str) {
    let before = source_file_observation(path).unwrap();
    for index in 0..1024 {
        fs::write(path.join(format!("{prefix}-{index}.jsonl")), b"new sibling").unwrap();
        if source_file_observation(path).unwrap() != before {
            return;
        }
    }
    panic!("fixture could not produce observable directory growth");
}

#[test]
fn gh512_legacy_rows_reuse_immediately_without_ignoring_explicit_dependencies() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("agent-a.jsonl");
    fs::write(&path, b"primary").unwrap();
    let rows: Vec<_> = ["claude_code", "codex", "shelley", "unknown-provider"]
        .into_iter()
        .map(|slug| {
            let source = discovered(&path, slug);
            let row = legacy_row(&source);
            assert!(source_ledger_matches(&row, &source));
            (source, row)
        })
        .collect();
    grow_directory(temp.path(), "sibling");
    for (source, row) in rows {
        assert_eq!(
            source_ledger_matches(&row, &source),
            matches!(source.provider_slug.as_str(), "claude_code" | "codex"),
            "{}",
            source.provider_slug,
        );
    }
    let source = discovered(&path, "claude_code");
    let sidecar = temp.path().join("explicit-metadata");
    fs::write(&sidecar, b"metadata").unwrap();
    let row = saved_row(&source, vec![source_file_observation(&sidecar).unwrap()]);
    fs::write(&sidecar, b"changed metadata").unwrap();
    assert!(!source_ledger_matches(&row, &source));
    let absent = temp.path().join("future-sidecar");
    let row = saved_row(&source, vec![source_file_observation(&absent).unwrap()]);
    fs::write(&absent, b"appeared").unwrap();
    assert!(!source_ledger_matches(&row, &source));
}

#[test]
fn gh512_primary_mutation_rejects_both_reuse_and_completion() {
    for slug in ["claude_code", "codex", "shelley"] {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("session.jsonl");
        fs::write(&path, b"original").unwrap();
        let source = discovered(&path, slug);
        let primary = source_file_observation(&path).unwrap();
        let parent = source_parent_observation(&source);
        let legacy = legacy_row(&source);
        let modern = saved_row(&source, Vec::new());
        fs::write(&path, b"changed primary transcript").unwrap();
        assert!(!source_ledger_matches(&legacy, &source));
        assert!(!source_ledger_matches(&modern, &source));
        assert!(
            source_observations_after_scan(
                &source,
                &[],
                Some(&primary),
                parent.as_ref(),
                &HashMap::new()
            )
            .is_none()
        );
    }
}

#[cfg(unix)]
#[test]
fn gh512_primary_replacement_with_same_size_and_mtime_is_not_a_sibling_change() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("session.jsonl");
    fs::write(&path, b"original").unwrap();
    let source = discovered(&path, "codex");
    let row = legacy_row(&source);
    let original_mtime = fs::metadata(&path).unwrap().modified().unwrap();
    let replacement = temp.path().join("replacement");
    fs::write(&replacement, b"replaced").unwrap();
    File::open(&replacement)
        .unwrap()
        .set_modified(original_mtime)
        .unwrap();
    fs::rename(&replacement, &path).unwrap();
    assert!(!source_ledger_matches(&row, &source));
}

#[test]
fn gh512_new_optional_sidecar_invalidates_even_when_previous_completion_had_none() {
    for slug in ["shelley", "kiro", "grok", "unknown-provider"] {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("sessions.sqlite3");
        fs::write(&path, b"database").unwrap();
        let source = discovered(&path, slug);
        let primary = source_file_observation(&path).unwrap();
        let parent = source_parent_observation(&source);
        let (_, dependencies) = source_observations_after_scan(
            &source,
            &[],
            Some(&primary),
            parent.as_ref(),
            &HashMap::new(),
        )
        .unwrap();
        assert_eq!(dependencies.len(), 1);
        let row = saved_row(&source, dependencies);
        fs::write(temp.path().join("sessions.sqlite3-wal"), b"new WAL").unwrap();
        grow_directory(temp.path(), "directory-clock");
        assert!(!source_ledger_matches(&row, &source));
        assert!(
            source_observations_after_scan(
                &source,
                &[],
                Some(&primary),
                parent.as_ref(),
                &HashMap::new()
            )
            .is_none()
        );
    }
}

#[test]
fn gh512_declared_sidecar_mutation_withholds_completion_for_every_policy() {
    for slug in ["claude_code", "codex", "shelley"] {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("primary");
        let sidecar_path = temp.path().join("sidecar");
        fs::write(&path, b"primary").unwrap();
        fs::write(&sidecar_path, b"sidecar").unwrap();
        let source = discovered(&path, slug);
        let sidecar = discovered(&sidecar_path, slug);
        let primary = source_file_observation(&path).unwrap();
        let parent = source_parent_observation(&source);
        let dependencies_before = HashMap::from([(
            sidecar_path.clone(),
            source_file_observation(&sidecar_path).unwrap(),
        )]);
        fs::write(&sidecar_path, b"mutated sidecar content").unwrap();
        assert!(
            source_observations_after_scan(
                &source,
                &[sidecar],
                Some(&primary),
                parent.as_ref(),
                &dependencies_before
            )
            .is_none()
        );
    }
}

#[test]
fn gh512_unobserved_sidecars_cannot_certify_a_completion() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("primary");
    let sidecar_path = temp.path().join("sidecar");
    fs::write(&path, b"primary").unwrap();
    fs::write(&sidecar_path, b"sidecar").unwrap();
    let source = discovered(&path, "claude_code");
    let sidecar = discovered(&sidecar_path, "claude_code");
    let primary = source_file_observation(&path).unwrap();
    assert!(
        source_observations_after_scan(
            &source,
            std::slice::from_ref(&sidecar),
            Some(&primary),
            None,
            &HashMap::new()
        )
        .is_none()
    );
    let dependencies_before = HashMap::from([(
        sidecar_path.clone(),
        source_file_observation(&sidecar_path).unwrap(),
    )]);
    let (_, dependencies) = source_observations_after_scan(
        &source,
        &[sidecar],
        Some(&primary),
        None,
        &dependencies_before,
    )
    .unwrap();
    assert_eq!(
        dependencies,
        vec![source_file_observation(&sidecar_path).unwrap()]
    );
}

#[test]
fn gh512_concurrent_folder_growth_preserves_only_self_contained_completions() {
    for slug in ["claude_code", "codex", "shelley", "unknown-provider"] {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("primary");
        fs::write(&path, b"unchanged source").unwrap();
        let source = discovered(&path, slug);
        let primary = source_file_observation(&path).unwrap();
        let parent = source_parent_observation(&source);
        let barrier = Arc::new(Barrier::new(2));
        let writer_barrier = Arc::clone(&barrier);
        let directory = temp.path().to_path_buf();
        let writer = thread::spawn(move || {
            writer_barrier.wait();
            grow_directory(&directory, "concurrent");
            writer_barrier.wait();
        });
        barrier.wait();
        barrier.wait();
        let observations = source_observations_after_scan(
            &source,
            &[],
            Some(&primary),
            parent.as_ref(),
            &HashMap::new(),
        );
        writer.join().unwrap();
        if matches!(slug, "claude_code" | "codex") {
            assert!(
                parent.is_none(),
                "self-contained scans must not stat the parent"
            );
            let (_, dependencies) =
                observations.expect("folder growth is unrelated to this source");
            assert!(dependencies.is_empty());
            let row = saved_row(&source, dependencies);
            grow_directory(temp.path(), "later");
            assert!(source_ledger_matches(&row, &source));
        } else {
            assert!(
                observations.is_none(),
                "an optional sidecar may have appeared"
            );
        }
    }
}

#[test]
fn gh512_legacy_handling_keeps_producer_and_malformed_row_guards() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("primary");
    fs::write(&path, b"primary").unwrap();
    let source = discovered(&path, "claude_code");
    let row = legacy_row(&source);
    let mut value: serde_json::Value = serde_json::from_str(&row).unwrap();
    value["producer_contract"] = serde_json::json!("unrelated-parser-contract");
    assert!(!source_ledger_matches(&value.to_string(), &source));
    for malformed in ["{}", "null", "{", r#"{"dependencies":[]}"#] {
        assert!(!source_ledger_matches(malformed, &source));
    }
    value = serde_json::from_str(&row).unwrap();
    value["dependencies"] = serde_json::json!([{"path": 7}]);
    assert!(!source_ledger_matches(&value.to_string(), &source));
}

#[test]
#[ignore = "GH512 workload-shaped native reuse benchmark; run explicitly with --ignored --nocapture"]
fn gh512_benchmark_4159_unchanged_sources_in_10_growing_folders() {
    const SOURCES: usize = 4159;
    const FOLDERS: usize = 10;
    const ROUNDS: usize = 7;
    let temp = TempDir::new().unwrap();
    let folders: Vec<_> = (0..FOLDERS)
        .map(|index| {
            let path = temp.path().join(format!("subagents-{index}"));
            fs::create_dir(&path).unwrap();
            path
        })
        .collect();
    let sources: Vec<_> = (0..SOURCES)
        .map(|index| {
            let path = folders[index % FOLDERS].join(format!("agent-{index}.jsonl"));
            fs::write(
                &path,
                b"{\"type\":\"user\",\"message\":{\"content\":\"benchmark\"}}\n",
            )
            .unwrap();
            discovered(
                &path,
                if index % 2 == 0 {
                    "claude_code"
                } else {
                    "codex"
                },
            )
        })
        .collect();
    let rows: Vec<_> = sources.iter().map(legacy_row).collect();
    for (source, row) in sources.iter().zip(&rows) {
        assert!(gh512_incumbent_source_ledger_matches(row, source));
        assert!(source_ledger_matches(row, source));
    }
    for folder in &folders {
        grow_directory(folder, "new-nightly-source");
    }
    let mut incumbent_times = Vec::new();
    let mut candidate_times = Vec::new();
    for round in 0..ROUNDS {
        // Alternate order so the same implementation does not always go last.
        for incumbent in [round % 2 == 0, round % 2 != 0] {
            let begin = Instant::now();
            let mut reused = 0;
            for (source, row) in sources.iter().zip(&rows) {
                let matches = if incumbent {
                    gh512_incumbent_source_ledger_matches(black_box(row), black_box(source))
                } else {
                    source_ledger_matches(black_box(row), black_box(source))
                };
                reused += usize::from(black_box(matches));
            }
            let nanos = begin.elapsed().as_nanos();
            if incumbent {
                assert_eq!(reused, 0);
                incumbent_times.push(nanos);
            } else {
                assert_eq!(reused, SOURCES);
                candidate_times.push(nanos);
            }
        }
    }
    incumbent_times.sort_unstable();
    candidate_times.sort_unstable();
    println!(
        "GH512_BENCH {}",
        serde_json::json!({
            "sources": SOURCES, "growing_folders": FOLDERS, "rounds": ROUNDS,
            "incumbent_reused": 0, "candidate_reused": SOURCES,
            "incumbent_forced_reparses": SOURCES, "candidate_forced_reparses": 0,
            "incumbent_median_ns": incumbent_times[ROUNDS / 2],
            "candidate_median_ns": candidate_times[ROUNDS / 2],
            "incumbent_samples_ns": incumbent_times,
            "candidate_samples_ns": candidate_times,
            "scope": "native ledger matching only; not a full nightly index benchmark",
            "debug_assertions": cfg!(debug_assertions),
        })
    );
}

// Verbatim pre-fix matcher for the same-invocation benchmark.
fn gh512_incumbent_source_ledger_matches(observation: &str, source: &DiscoveredSourceFile) -> bool {
    let Ok(saved) = serde_json::from_str::<serde_json::Value>(observation) else {
        return false;
    };
    if saved["producer_contract"].as_str() != Some(env!("CASS_SOURCE_INGEST_CONTRACT")) {
        return false;
    }
    if saved["primary"] != source_file_observation(&source.source_path).unwrap_or_default() {
        return false;
    }
    let Some(files) = saved["dependencies"].as_array() else {
        return false;
    };
    files.iter().all(|file| {
        file["path"]
            .as_str()
            .is_some_and(|path| source_file_observation(Path::new(path)).as_ref() == Some(file))
    })
}

/// Count the indexer's dependency preflight separately from the connector's
/// own discovery inside scan_with_source_boundaries.
struct LedgerDiscoveryCounter {
    inner: Box<dyn Connector + Send>,
    discoveries: std::cell::Cell<usize>,
}

impl Connector for LedgerDiscoveryCounter {
    fn detect(&self) -> crate::connectors::DetectionResult {
        self.inner.detect()
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        self.inner.scan(ctx)
    }

    fn supports_source_boundaries(&self) -> bool {
        self.inner.supports_source_boundaries()
    }

    fn discover_source_files(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        self.discoveries.set(self.discoveries.get() + 1);
        self.inner.discover_source_files(ctx)
    }

    fn scan_with_source_boundaries(
        &self,
        ctx: &ScanContext,
        hooks: &mut franken_agent_detection::SourceScanHooks<'_>,
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        self.inner
            .scan_with_source_boundaries(ctx, hooks, on_conversation)
    }
}

#[test]
fn gh512_real_connectors_complete_during_growth_without_redundant_discovery() -> Result<()> {
    let factories: Vec<_> = crate::connectors::get_connector_factories()
        .into_iter()
        .filter(|(name, _)| matches!(*name, "claude" | "codex"))
        .collect();
    assert_eq!(factories.len(), 2);
    for (name, factory) in factories {
        let temp = TempDir::new()?;
        let source_path = if name == "claude" {
            temp.path().join("claude/projects/gh512/session.jsonl")
        } else {
            temp.path()
                .join("codex/sessions/2026/10/06/rollout-gh512.jsonl")
        };
        let parent = source_path.parent().unwrap().to_path_buf();
        fs::create_dir_all(&parent)?;
        let transcript = if name == "claude" {
            concat!(
                "{\"type\":\"user\",\"sessionId\":\"gh512\",\"uuid\":\"gh512-message\",",
                "\"timestamp\":\"2026-08-01T10:00:00Z\",\"cwd\":\"/work/gh512\",",
                "\"message\":{\"role\":\"user\",\"content\":\"retained transcript\"}}\n",
            )
        } else {
            concat!(
                "{\"timestamp\":\"2026-08-01T10:00:00Z\",\"type\":\"session_meta\",",
                "\"payload\":{\"id\":\"gh512\",\"cwd\":\"/work/gh512\"}}\n",
                "{\"timestamp\":\"2026-08-01T10:00:01Z\",\"type\":\"response_item\",",
                "\"payload\":{\"type\":\"message\",\"role\":\"user\",",
                "\"content\":[{\"type\":\"input_text\",\"text\":\"retained transcript\"}]}}\n",
            )
        };
        fs::write(&source_path, transcript)?;
        let data_dir = temp.path().join("data");
        fs::create_dir(&data_dir)?;
        let ctx = ScanContext::with_roots(
            data_dir.clone(),
            vec![ScanRoot::local(source_path.clone())],
            None,
        );
        let limiter = Arc::new(StreamingByteLimiter::new(STREAMING_MAX_BYTES_IN_FLIGHT));
        let mut config = StreamingProducerConfig {
            source_ledger: Arc::new(HashMap::new()),
            flow_limiter: Arc::clone(&limiter),
            data_dir,
            additional_scan_roots: Vec::new(),
            local_connector_roots: None,
            since_ts: None,
            local_since_ts_by_connector: Arc::new(HashMap::new()),
            progress: None,
            active_source_filter: Arc::new(ActiveSessionSourceFilter::default()),
        };
        let connector = LedgerDiscoveryCounter {
            inner: factory(),
            discoveries: std::cell::Cell::new(0),
        };
        let (tx, rx) = bounded(8);
        let mut sender = StreamingBatchSender::new(&tx, Arc::clone(&limiter), name, true);
        let mut emitted = 0;
        scan_with_durable_source_boundaries(
            &connector,
            &ctx,
            &config,
            &mut sender,
            |conversation| {
                emitted += 1;
                // This writer runs between the real pre-parse and completion hooks.
                thread::scope(|scope| {
                    scope
                        .spawn(|| grow_directory(&parent, "during-parse"))
                        .join()
                        .unwrap();
                });
                Ok(Some(conversation))
            },
        )?;
        sender.flush()?;
        assert_eq!(emitted, 1, "{name}");
        assert_eq!(
            connector.discoveries.get(),
            0,
            "{name}: no sidecar inventory needed"
        );
        let IndexMessage::SourceComplete {
            completion,
            conversations,
            byte_reservation,
            ..
        } = rx.try_recv()?
        else {
            panic!("{name}: folder growth must not discard the durable source marker");
        };
        assert_eq!(conversations.len(), 1);
        let observation: serde_json::Value = serde_json::from_str(&completion.observation)?;
        assert_eq!(observation["dependencies"], serde_json::json!([]));
        drop(conversations);
        limiter.release(byte_reservation);
        assert!(rx.try_recv().is_err());
        config.source_ledger = Arc::new(HashMap::from([(completion.key, completion.observation)]));
        grow_directory(&parent, "after-completion");
        let mut rescanned = 0;
        scan_with_durable_source_boundaries(
            &connector,
            &ctx,
            &config,
            &mut sender,
            |conversation| {
                rescanned += 1;
                Ok(Some(conversation))
            },
        )?;
        sender.flush()?;
        let reusable = env!("CASS_SOURCE_INGEST_REUSE") == "true";
        assert_eq!(rescanned, usize::from(!reusable), "{name}");
        assert_eq!(connector.discoveries.get(), 0);
        if reusable {
            assert!(rx.try_recv().is_err());
        } else {
            let IndexMessage::SourceComplete {
                conversations,
                byte_reservation,
                ..
            } = rx.try_recv()?
            else {
                panic!("mutable dependency replay must still certify stable source bytes");
            };
            drop(conversations);
            limiter.release(byte_reservation);
        }
        // Unknown registrations keep the old conservative discovery path,
        // even when a test adapter happens to delegate to a single-file parser.
        let mut conservative =
            StreamingBatchSender::new(&tx, Arc::clone(&limiter), "future-connector", true);
        scan_with_durable_source_boundaries(
            &connector,
            &ctx,
            &config,
            &mut conservative,
            |conversation| Ok(Some(conversation)),
        )?;
        conservative.flush()?;
        assert_eq!(
            connector.discoveries.get(),
            1,
            "unknown connector must still discover dependencies"
        );
        while let Ok(message) = rx.try_recv() {
            match message {
                IndexMessage::Batch {
                    conversations,
                    byte_reservation,
                    ..
                }
                | IndexMessage::SourceComplete {
                    conversations,
                    byte_reservation,
                    ..
                } => {
                    drop(conversations);
                    limiter.release(byte_reservation);
                }
                _ => panic!("unexpected producer message"),
            }
        }
        // Make the primary differ from the saved row, then mutate it again
        // after parsing: only an ordinary batch may cross this boundary.
        fs::write(&source_path, format!("{transcript}\n"))?;
        scan_with_durable_source_boundaries(
            &connector,
            &ctx,
            &config,
            &mut sender,
            |conversation| {
                fs::write(&source_path, format!("{transcript}\n\n"))?;
                Ok(Some(conversation))
            },
        )?;
        sender.flush()?;
        let IndexMessage::Batch {
            conversations,
            byte_reservation,
            ..
        } = rx.try_recv()?
        else {
            panic!("{name}: changing primary bytes must never certify a completion");
        };
        drop(conversations);
        limiter.release(byte_reservation);
        assert!(rx.try_recv().is_err());

        // Whole-source exclusion capability never certifies a projection that
        // the host actually filtered. A later unfiltered attempt must still run.
        config.source_ledger = Arc::new(HashMap::new());
        let mut filtered_count = 0;
        scan_with_durable_source_boundaries(
            &connector,
            &ctx,
            &config,
            &mut sender,
            |_| {
                filtered_count += 1;
                Ok(None)
            },
        )?;
        sender.flush()?;
        assert_eq!(filtered_count, 1);
        assert!(
            rx.try_recv().is_err(),
            "filtered content must not acquire a completion"
        );
        scan_with_durable_source_boundaries(
            &connector,
            &ctx,
            &config,
            &mut sender,
            |conversation| Ok(Some(conversation)),
        )?;
        sender.flush()?;
        let IndexMessage::SourceComplete {
            conversations,
            byte_reservation,
            ..
        } = rx.try_recv()?
        else {
            panic!("a stable unfiltered retry must acquire the completion");
        };
        assert_eq!(conversations.len(), 1);
        drop(conversations);
        limiter.release(byte_reservation);
        assert!(rx.try_recv().is_err());
    }
    Ok(())
}

/// The real Grok parser selects existing sidecars BEFORE calling should_scan.
/// Create a summary in precisely that gap, without replacing discovery or parsing.
struct GrokSummaryAtAdmission {
    summary: PathBuf,
    injected: std::cell::Cell<bool>,
}

impl Connector for GrokSummaryAtAdmission {
    fn detect(&self) -> crate::connectors::DetectionResult {
        crate::connectors::grok::GrokConnector::new().detect()
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        crate::connectors::grok::GrokConnector::new().scan(ctx)
    }

    fn discover_source_files(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        crate::connectors::grok::GrokConnector::new().discover_source_files(ctx)
    }

    fn supports_source_boundaries(&self) -> bool {
        true
    }

    fn scan_with_source_boundaries(
        &self,
        ctx: &ScanContext,
        hooks: &mut franken_agent_detection::SourceScanHooks<'_>,
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        let original_should_scan = &mut hooks.should_scan_source;
        let mut inject = |source: &DiscoveredSourceFile| {
            if !self.injected.replace(true) {
                fs::write(&self.summary, r#"{"generated_title":"late summary"}"#).unwrap();
                grow_directory(self.summary.parent().unwrap(), "admission-clock");
            }
            original_should_scan
                .as_mut()
                .is_none_or(|callback| callback(source))
        };
        // Forward completion through a local closure too: both hooks must
        // share this call's lifetime, which the caller's hooks outlive.
        let original_complete = &mut hooks.on_source_complete;
        let mut complete = |completion: &franken_agent_detection::connectors::SourceCompletion| {
            original_complete
                .as_mut()
                .map_or(Ok(()), |callback| callback(completion))
        };
        let mut forwarded = franken_agent_detection::SourceScanHooks {
            should_scan_source: Some(&mut inject),
            on_source_complete: Some(&mut complete),
        };
        crate::connectors::grok::GrokConnector::new().scan_with_source_boundaries(
            ctx,
            &mut forwarded,
            on_conversation,
        )
    }
}

fn collect_ledger_scan(
    connector: &dyn Connector,
    ctx: &ScanContext,
    config: &StreamingProducerConfig,
    name: &'static str,
) -> Result<(
    Vec<NormalizedConversation>,
    Vec<crate::storage::sqlite::SourceIngestLedgerEntry>,
)> {
    let limiter = Arc::clone(&config.flow_limiter);
    let (tx, rx) = bounded(8);
    let mut sender = StreamingBatchSender::new(&tx, Arc::clone(&limiter), name, true);
    scan_with_durable_source_boundaries(connector, ctx, config, &mut sender, |conversation| {
        Ok(Some(conversation))
    })?;
    sender.flush()?;
    let mut conversations = Vec::new();
    let mut entries = Vec::new();
    for message in rx.try_iter() {
        match message {
            IndexMessage::Batch {
                conversations: batch,
                byte_reservation,
                ..
            } => {
                conversations.extend(batch);
                limiter.release(byte_reservation);
            }
            IndexMessage::SourceComplete {
                conversations: batch,
                completion,
                byte_reservation,
                ..
            } => {
                conversations.extend(batch);
                entries.push(completion);
                limiter.release(byte_reservation);
            }
            _ => panic!("unexpected source producer message"),
        }
    }
    Ok((conversations, entries))
}

#[test]
fn gh512_real_sidecar_appearance_before_admission_cannot_certify_an_untracked_file() -> Result<()> {
    let temp = TempDir::new()?;
    let directory = temp.path().join("grok/sessions/%2Fwork%2Fledger/session");
    fs::create_dir_all(&directory)?;
    let primary = directory.join("updates.jsonl");
    fs::write(
        &primary,
        format!(
            "{}\n",
            serde_json::json!({
                "timestamp": 1_784_388_056,
                "method": "session/update",
                "params": {"sessionId": "ledger-grok", "update": {
                    "sessionUpdate": "user_message_chunk",
                    "content": {"type": "text", "text": "retained Grok transcript"}
                }}
            })
        ),
    )?;
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir)?;
    let ctx = ScanContext::with_roots(
        data_dir.clone(),
        vec![ScanRoot::local(directory.clone())],
        None,
    );
    let mut config = StreamingProducerConfig {
        source_ledger: Arc::new(HashMap::new()),
        flow_limiter: Arc::new(StreamingByteLimiter::new(STREAMING_MAX_BYTES_IN_FLIGHT)),
        data_dir,
        additional_scan_roots: Vec::new(),
        local_connector_roots: None,
        since_ts: None,
        local_since_ts_by_connector: Arc::new(HashMap::new()),
        progress: None,
        active_source_filter: Arc::new(ActiveSessionSourceFilter::default()),
    };
    let connector = GrokSummaryAtAdmission {
        summary: directory.join("summary.json"),
        injected: std::cell::Cell::new(false),
    };
    let primary_before = source_file_observation(&primary).unwrap();
    let (conversations, entries) = collect_ledger_scan(&connector, &ctx, &config, "grok")?;
    assert_eq!(conversations.len(), 1);
    assert_eq!(conversations[0].title.as_deref(), Some("late summary"));
    assert_eq!(
        source_file_observation(&primary),
        Some(primary_before.clone())
    );
    assert!(
        entries.is_empty(),
        "a parsed but untracked sidecar must withhold completion"
    );

    // A subsequent stable scan discovers and records the sidecar normally.
    let (conversations, mut entries) = collect_ledger_scan(&connector, &ctx, &config, "grok")?;
    assert_eq!(conversations.len(), 1);
    assert_eq!(entries.len(), 1);
    let entry = entries.pop().unwrap();
    let saved: serde_json::Value = serde_json::from_str(&entry.observation)?;
    let dependencies = saved["dependencies"].as_array().unwrap();
    assert!(dependencies.contains(&source_file_observation(&directory).unwrap()));
    assert!(dependencies.contains(&source_file_observation(&connector.summary).unwrap()));
    let source = discovered(&primary, "grok");
    assert!(source_ledger_matches(&entry.observation, &source));
    config.source_ledger = Arc::new(HashMap::from([(entry.key, entry.observation)]));

    // An in-place edit does NOT change the folder. Without the explicit
    // sidecar observation, the wrongly certified first scan would now skip it.
    let parent_before = source_file_observation(&directory).unwrap();
    fs::write(
        &connector.summary,
        r#"{"generated_title":"updated metadata title"}"#,
    )?;
    assert_eq!(source_file_observation(&directory), Some(parent_before));
    assert_eq!(source_file_observation(&primary), Some(primary_before));
    let (conversations, mut entries) = collect_ledger_scan(&connector, &ctx, &config, "grok")?;
    assert_eq!(conversations.len(), 1);
    assert_eq!(
        conversations[0].title.as_deref(),
        Some("updated metadata title")
    );
    assert_eq!(entries.len(), 1);
    let entry = entries.pop().unwrap();
    config.source_ledger = Arc::new(HashMap::from([(entry.key, entry.observation)]));
    let (conversations, entries) = collect_ledger_scan(&connector, &ctx, &config, "grok")?;
    let expected = usize::from(env!("CASS_SOURCE_INGEST_REUSE") != "true");
    assert_eq!(conversations.len(), expected);
    assert_eq!(entries.len(), expected);

    fs::remove_file(&connector.summary)?;
    let (conversations, entries) = collect_ledger_scan(&connector, &ctx, &config, "grok")?;
    assert_eq!(
        conversations.len(),
        1,
        "sidecar deletion must also invalidate reuse"
    );
    assert_ne!(
        conversations[0].title.as_deref(),
        Some("updated metadata title")
    );
    assert_eq!(entries.len(), 1);
    Ok(())
}
