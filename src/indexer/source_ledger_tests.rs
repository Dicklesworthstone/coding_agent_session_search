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

#[cfg(unix)]
fn codex_prefix_fixture(temp: &TempDir) -> Result<(PathBuf, ScanContext, StreamingProducerConfig)> {
    let path = temp.path().join("rollout-ledger-prefix.jsonl");
    fs::write(&path, b"{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":\"first\"}}\n")?;
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir)?;
    let ctx = ScanContext::with_roots(data_dir.clone(), vec![ScanRoot::local(path.clone())], None);
    Ok((path, ctx, diagnostic_ledger_config(data_dir)))
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn claude_prefix_startup_retains_partial_identity_and_replays_only_proven_history() -> Result<()> {
    let temp = TempDir::new()?;
    let path = temp.path().join("claude-ledger-prefix.jsonl");
    let first = b"{\"type\":\"user\",\"uuid\":\"first\",\"message\":{\"role\":\"user\",\"content\":\"first stable turn\"}}\n";
    fs::write(&path, first)?;
    let mut writer = OpenOptions::new().append(true).open(&path)?;
    let data = temp.path().join("data");
    fs::create_dir(&data)?;
    let ctx = ScanContext::with_roots(data.clone(), vec![ScanRoot::local(path.clone())], None);
    let mut config = diagnostic_ledger_config(data);
    let connector = crate::connectors::claude_code::ClaudeCodeConnector::new();
    let (initial, mut entries) = collect_ledger_scan(&connector, &ctx, &config, "claude")?;
    assert_eq!(initial.len(), 1);
    assert_eq!(initial[0].messages.len(), 1);
    let entry = entries.pop().context("Claude startup receipt missing")?;
    let saved: serde_json::Value = serde_json::from_str(&entry.observation)?;
    assert!(saved.get("codex_prefix").is_none());
    assert_eq!(saved["claude_prefix"]["provider"], "claude_code");
    assert_eq!(saved["claude_prefix"]["partial"], true);
    assert!(!source_ledger_matches(
        &entry.observation,
        &discovered(&path, "claude_code")
    ));
    assert!(
        config
            .active_source_filter
            .take_deferred_sources()
            .contains(&path)
    );
    config.source_ledger = Arc::new(HashMap::from([(entry.key, entry.observation)]));
    writer.write_all(b"{\"type\":\"assistant\",\"uuid\":\"second\",\"message\":{\"role\":\"assistant\",\"content\":\"second stable turn\"}}\n")?;
    let (next, entries) = collect_ledger_scan(&connector, &ctx, &config, "claude")?;
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].messages.len(), 2);
    assert_eq!(next[0].external_id, initial[0].external_id);
    assert_eq!(next[0].source_path, path);
    assert_eq!(next[0].messages[0].content, initial[0].messages[0].content);
    assert_eq!(entries.len(), 1);
    let (replayed, _) = collect_ledger_scan(&connector, &ctx, &config, "claude")?;
    assert_eq!(replayed[0].external_id, next[0].external_id);
    fs::write(
        &path,
        fs::read_to_string(&path)?.replace("first stable", "other stable"),
    )?;
    assert!(collect_ledger_scan(&connector, &ctx, &config, "claude").is_err());
    Ok(())
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn claude_prefix_startup_discards_source_changed_in_delivery_callback() -> Result<()> {
    let temp = TempDir::new()?;
    let path = temp.path().join("claude-queued-prefix.jsonl");
    fs::write(
        &path,
        b"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"first turn\"}}\n",
    )?;
    let data = temp.path().join("data");
    fs::create_dir(&data)?;
    let ctx = ScanContext::with_roots(data.clone(), vec![ScanRoot::local(path.clone())], None);
    let config = diagnostic_ledger_config(data);
    let (tx, rx) = bounded(8);
    let mut sender =
        StreamingBatchSender::new(&tx, Arc::clone(&config.flow_limiter), "claude", true);
    let result = scan_with_durable_source_boundaries(
        &crate::connectors::claude_code::ClaudeCodeConnector::new(),
        &ctx,
        &config,
        &mut sender,
        |conversation| {
            fs::write(&path, fs::read_to_string(&path)?.replace("first", "other"))?;
            Ok(Some(conversation))
        },
    );
    assert!(result.is_err());
    sender.flush()?;
    assert!(rx.try_iter().next().is_none());
    assert!(sender.partial_sources_observed);
    assert!(
        config
            .active_source_filter
            .take_deferred_sources()
            .contains(&path)
    );
    Ok(())
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn codex_prefix_startup_keeps_partial_receipts_and_rediscovers_appends() -> Result<()> {
    let temp = TempDir::new()?;
    let (path, ctx, mut config) = codex_prefix_fixture(&temp)?;
    let mut writer = OpenOptions::new().append(true).open(&path)?;
    let connector = crate::connectors::codex::CodexConnector::new();
    let (first, mut entries) = collect_ledger_scan(&connector, &ctx, &config, "codex")?;
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].messages.len(), 1);
    let entry = entries.pop().context("first prefix receipt")?;
    let value: serde_json::Value = serde_json::from_str(&entry.observation)?;
    assert_eq!(value["codex_prefix"]["partial"], true);
    assert!(!source_ledger_matches(
        &entry.observation,
        &discovered(&path, "codex")
    ));
    assert!(
        config
            .active_source_filter
            .take_deferred_sources()
            .contains(&path)
    );
    config.source_ledger = Arc::new(HashMap::from([(entry.key, entry.observation)]));
    writer.write_all(b"{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":\"second\"}}\n")?;
    let (next, entries) = collect_ledger_scan(&connector, &ctx, &config, "codex")?;
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].messages.len(), 2);
    assert_eq!(next[0].messages[0].content, "first");
    assert_eq!(next[0].messages[1].content, "second");
    assert_eq!(next[0].external_id, first[0].external_id);
    assert_eq!(entries.len(), 1);
    Ok(())
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn codex_prefix_startup_discards_delivery_rejected_by_final_validation() -> Result<()> {
    let temp = TempDir::new()?;
    let (path, ctx, config) = codex_prefix_fixture(&temp)?;
    let connector = crate::connectors::codex::CodexConnector::new();
    let (tx, rx) = bounded(8);
    let mut sender =
        StreamingBatchSender::new(&tx, Arc::clone(&config.flow_limiter), "codex", true);
    let result = scan_with_durable_source_boundaries(
        &connector,
        &ctx,
        &config,
        &mut sender,
        |conversation| {
            let bytes = fs::read_to_string(&path)?.replace("first", "other");
            fs::write(&path, bytes)?;
            Ok(Some(conversation))
        },
    );
    assert!(
        result.is_err(),
        "the real adapter must reject the post-delivery mutation"
    );
    sender.flush()?;
    assert!(
        rx.try_iter().next().is_none(),
        "no Batch fallback may publish the rejected view"
    );
    assert!(sender.partial_sources_observed);
    assert!(
        config
            .active_source_filter
            .take_deferred_sources()
            .contains(&path)
    );
    Ok(())
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn codex_prefix_startup_refused_proof_does_not_starve_later_siblings() -> Result<()> {
    use crate::connector_ingest_diagnostics::IngestFailureKind;
    for refusal in ["mutation", "truncation", "replacement", "malformed"] {
        for remote in [false, true] {
            let temp = TempDir::new()?;
            let (refused, _, mut config) = codex_prefix_fixture(&temp)?;
            let root = if remote {
                ScanRoot::remote(temp.path().to_path_buf(), Origin::remote("proof-test"), None)
            } else {
                ScanRoot::local(temp.path().to_path_buf())
            };
            let ctx = ScanContext::with_roots(config.data_dir.clone(), vec![root], None);
            let connector = crate::connectors::codex::CodexConnector::new();
            let (_, mut entries) = collect_ledger_scan(&connector, &ctx, &config, "codex")?;
            let mut entry = entries.pop().context("initial prior proof")?;
            let original = fs::read_to_string(&refused)?;
            match refusal {
                "mutation" => fs::write(&refused, original.replace("first", "other"))?,
                "truncation" => fs::write(&refused, b"{")?,
                "replacement" => {
                    fs::rename(&refused, refused.with_extension("retained"))?;
                    fs::write(&refused, &original)?;
                }
                "malformed" => entry.observation = serde_json::json!({
                    "codex_prefix": {"schema_version": 0},
                }).to_string(),
                _ => unreachable!(),
            }
            let healthy = temp.path().join("rollout-z-healthy.jsonl");
            fs::write(&healthy, &original)?;
            File::open(&healthy)?.set_modified(SystemTime::now() - Duration::from_secs(7200))?;
            let inventory = connector.discover_source_files(&ctx)?;
            assert_eq!(inventory[0].source_path, refused, "bad source must precede its sibling");
            let saved = HashMap::from([(entry.key.clone(), entry.observation.clone())]);
            config.source_ledger = Arc::new(saved.clone());
            config.incomplete_remote_sources.store(false, Ordering::Relaxed);
            config.active_source_filter.take_deferred_sources();
            let scan = collect_diagnostic_ledger_scan(&connector, &ctx, &config, "codex", || Ok(()))?;
            let error = scan.scan_error.as_ref().context("refused proof certified a complete scan")?;
            assert_eq!(scan.conversations.len(), 1, "{refusal}, remote={remote}");
            assert_eq!(scan.conversations[0].source_path, healthy);
            assert_eq!(scan.entries.len(), 1);
            assert_ne!(scan.entries[0].key, entry.key);
            let mut failed_paths = Vec::new();
            assert!(crate::connectors::codex::for_each_source_failure(error, |path, _| {
                failed_paths.push(path.to_path_buf());
            }));
            assert_eq!(failed_paths, vec![refused.clone()]);
            let expected = if refusal == "malformed" {
                IngestFailureKind::UnparseableSource
            } else {
                IngestFailureKind::SourceChanged
            };
            assert!(scan.report.diagnostics.iter().any(|diagnostic| {
                diagnostic.source_path == refused.display().to_string()
                    && diagnostic.failure_kind == expected
            }));
            assert_eq!(scan.report.summary.total(), 2);
            assert_eq!(scan.report.summary.with_content(), 1);
            assert!(config.active_source_filter.take_deferred_sources().contains(&refused));
            assert_eq!(config.incomplete_remote_sources.load(Ordering::Relaxed), remote);
            assert_eq!(*config.source_ledger, saved, "refusal must preserve prior evidence");
        }
    }
    Ok(())
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn codex_prefix_startup_reports_admission_and_parser_failures_at_their_sources() -> Result<()> {
    use crate::connectors::codex::SourceFailureReason;
    let temp = TempDir::new()?;
    let (refused, _, mut config) = codex_prefix_fixture(&temp)?;
    let parser_failed = temp.path().join("rollout-m-parser-failure.jsonl");
    let healthy = temp.path().join("rollout-z-healthy.jsonl");
    fs::write(&parser_failed, b"\xff\n")?;
    fs::write(&healthy, fs::read(&refused)?)?;
    let ctx = ScanContext::with_roots(config.data_dir.clone(),
        vec![ScanRoot::local(temp.path().to_path_buf())], None);
    let connector = crate::connectors::codex::CodexConnector::new();
    let source = connector.discover_source_files(&ctx)?.into_iter()
        .find(|source| source.source_path == refused).context("refused source")?;
    config.source_ledger = Arc::new(HashMap::from([(
        source_ledger_key(&source, &ctx).context("source key")?,
        serde_json::json!({"codex_prefix": "malformed"}).to_string(),
    )]));
    let scan = collect_diagnostic_ledger_scan(&connector, &ctx, &config, "codex", || Ok(()))?;
    assert_eq!(scan.conversations.len(), 1);
    assert_eq!(scan.conversations[0].source_path, healthy);
    let error = scan.scan_error.as_ref().context("mixed failures were lost")?;
    let mut failures = Vec::new();
    assert!(crate::connectors::codex::for_each_source_failure(error, |path, reason| {
        failures.push((path.to_path_buf(), reason));
    }));
    assert_eq!(failures.len(), 2);
    for path in [refused, parser_failed] {
        assert!(failures.contains(&(path.clone(), SourceFailureReason::InvalidSourceData)));
        assert!(scan.report.diagnostics.iter().any(|diagnostic| {
            diagnostic.source_path == path.display().to_string()
                && diagnostic.failure_kind == crate::connector_ingest_diagnostics::IngestFailureKind::UnparseableSource
        }));
    }
    assert_eq!(scan.report.summary.total(), 3);
    assert_eq!(scan.report.summary.with_content(), 1);
    Ok(())
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn codex_prefix_startup_consumer_failure_remains_terminal_after_source_refusal() -> Result<()> {
    let temp = TempDir::new()?;
    let (refused, _, mut config) = codex_prefix_fixture(&temp)?;
    let first = temp.path().join("rollout-y-delivery-failure.jsonl");
    let later = temp.path().join("rollout-z-must-not-deliver.jsonl");
    for path in [&first, &later] {
        fs::write(path, fs::read(&refused)?)?;
    }
    let ctx = ScanContext::with_roots(config.data_dir.clone(),
        vec![ScanRoot::local(temp.path().to_path_buf())], None);
    let connector = crate::connectors::codex::CodexConnector::new();
    let source = connector.discover_source_files(&ctx)?.into_iter()
        .find(|source| source.source_path == refused).context("refused source")?;
    config.source_ledger = Arc::new(HashMap::from([(
        source_ledger_key(&source, &ctx).context("source key")?,
        serde_json::json!({"codex_prefix": "malformed"}).to_string(),
    )]));
    let (tx, rx) = bounded(8);
    drop(rx);
    let mut sender = StreamingBatchSender::new(&tx, Arc::clone(&config.flow_limiter), "codex", true);
    let mut prepared = Vec::new();
    let error = scan_with_durable_source_boundaries(&connector, &ctx, &config, &mut sender, |conversation| {
        prepared.push(conversation.source_path.clone());
        Ok(Some(conversation))
    }).err().context("disconnected consumer was hidden")?;
    assert!(is_streaming_consumer_disconnected(&error));
    assert_eq!(prepared, vec![first]);
    assert!(config.active_source_filter.take_deferred_sources().contains(&refused));
    Ok(())
}
#[cfg(unix)]
#[test]
#[serial_test::serial]
fn codex_prefix_full_rebuild_preserves_overlap_but_disables_reuse() -> Result<()> {
    let temp = TempDir::new()?;
    let (path, ctx, config) = codex_prefix_fixture(&temp)?;
    let (_, mut entries) = collect_ledger_scan(
        &crate::connectors::codex::CodexConnector::new(),
        &ctx,
        &config,
        "codex",
    )?;
    let entry = entries.pop().context("prefix receipt")?;
    let original: serde_json::Value = serde_json::from_str(&entry.observation)?;
    let preserved = prefix_integrity_observations(HashMap::from([
        (entry.key.clone(), entry.observation),
        (
            "ordinary".to_string(),
            saved_row(&discovered(&path, "codex"), Vec::new()),
        ),
    ]));
    assert_eq!(preserved.len(), 1);
    let marker = &preserved[&entry.key];
    let value: serde_json::Value = serde_json::from_str(marker)?;
    assert_eq!(value["codex_prefix"], original["codex_prefix"]);
    assert_eq!(value["publication_invalidated"], true);
    assert_eq!(value["watch_published"], false);
    assert!(!source_ledger_matches(marker, &discovered(&path, "codex")));
    fs::write(&path, fs::read_to_string(&path)?.replace("first", "other"))?;
    assert!(crate::connectors::codex::set_previous_prefix_receipt(&path, Some(marker)).is_err());
    Ok(())
}

#[test]
fn codex_prefix_invalidation_bounds_input_before_json_parsing() {
    // This is invalid JSON as well as oversized. The byte budget must reject
    // it before serde attempts to parse or allocate a second representation.
    let oversized = "{".repeat(256 * 1024 + 1);
    let error = bounded_rebuild_prefix_observation(&oversized).unwrap_err();
    assert!(error.to_string().contains("invalidation budget"));
    let ordinary = serde_json::json!({"dependencies": "x".repeat(256 * 1024)}).to_string();
    assert!(invalidated_prefix_observation(&ordinary).unwrap().is_none());
    assert!(bounded_rebuild_prefix_observation(&ordinary).is_err());
}

#[test]
fn codex_prefix_rebuild_keeps_unusable_integrity_keys_fail_closed() -> Result<()> {
    let mut boundary = serde_json::json!({"codex_prefix": {}, "padding": ""});
    let empty_len = boundary.to_string().len();
    boundary["padding"] = serde_json::json!("x".repeat(256 * 1024 - empty_len));
    let boundary = boundary.to_string();
    assert_eq!(boundary.len(), 256 * 1024);
    assert!(invalidated_prefix_observation(&boundary).is_err());

    let valid = serde_json::json!({"codex_prefix": {"retained_proof": true}});
    let rebuilt = prefix_integrity_observations(HashMap::from([
        ("valid".to_owned(), valid.to_string()),
        ("ordinary".to_owned(), "{\"primary\":{}}".to_owned()),
        ("malformed".to_owned(), "{\"codex_prefix\":".to_owned()),
        ("oversized".to_owned(), "{".repeat(256 * 1024 + 1)),
        ("grows_past_bound".to_owned(), boundary),
        ("null".to_owned(), "null".to_owned()),
        ("boolean".to_owned(), "false".to_owned()),
        ("string".to_owned(), "\"broken\"".to_owned()),
        ("array".to_owned(), "[]".to_owned()),
    ]));
    assert_eq!(rebuilt.len(), 8);
    assert!(!rebuilt.contains_key("ordinary"));
    let preserved: serde_json::Value = serde_json::from_str(&rebuilt["valid"])?;
    assert_eq!(preserved["codex_prefix"], valid["codex_prefix"]);
    assert_eq!(preserved["publication_invalidated"], true);
    assert_eq!(preserved["watch_published"], false);
    for key in ["malformed", "oversized", "grows_past_bound", "null", "boolean", "string", "array"] {
        let marker = &rebuilt[key];
        assert!(marker.len() < 128, "unusable evidence must remain bounded");
        assert!(
            crate::connectors::codex::set_previous_prefix_receipt(
                Path::new("rollout-refused-rebuild.jsonl"),
                Some(marker),
            )
            .is_err(),
            "the actual admission parser must reject {key} instead of forgetting its proof"
        );
        let value: serde_json::Value = serde_json::from_str(marker)?;
        assert_eq!(value["publication_invalidated"], true);
        assert_eq!(value["watch_published"], false);
    }
    Ok(())
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn codex_prefix_full_rebuild_unusable_receipt_does_not_starve_siblings() -> Result<()> {
    for malformed in [false, true] {
        for remote in [false, true] {
            let temp = TempDir::new()?;
            let (refused, _, mut config) = codex_prefix_fixture(&temp)?;
            let healthy = temp.path().join("rollout-z-rebuild-healthy.jsonl");
            fs::write(&healthy, fs::read(&refused)?)?;
            File::open(&healthy)?.set_modified(SystemTime::now() - Duration::from_secs(7200))?;
            let root = if remote {
                ScanRoot::remote(temp.path().to_path_buf(), Origin::remote("rebuild-proof"), None)
            } else {
                ScanRoot::local(temp.path().to_path_buf())
            };
            let ctx = ScanContext::with_roots(config.data_dir.clone(), vec![root], None);
            let connector = crate::connectors::codex::CodexConnector::new();
            let sources = connector.discover_source_files(&ctx)?;
            assert_eq!(sources[0].source_path, refused);
            let refused_key = source_ledger_key(&sources[0], &ctx).context("refused key")?;
            let healthy_source = sources.iter().find(|source| source.source_path == healthy)
                .context("healthy source")?;
            let healthy_key = source_ledger_key(healthy_source, &ctx).context("healthy key")?;
            let invalid = if malformed {
                "{\"codex_prefix\":".to_owned()
            } else {
                serde_json::json!({"codex_prefix": {}, "padding": "x".repeat(256 * 1024)}).to_string()
            };
            config.source_ledger = Arc::new(prefix_integrity_observations(HashMap::from([
                (refused_key.clone(), invalid),
                (healthy_key.clone(), saved_row(healthy_source, Vec::new())),
            ])));
            let scan = collect_diagnostic_ledger_scan(&connector, &ctx, &config, "codex", || Ok(()))?;
            let error = scan.scan_error.as_ref().context("unusable proof was forgotten")?;
            assert_eq!(scan.conversations.len(), 1, "malformed={malformed}, remote={remote}");
            assert_eq!(scan.conversations[0].source_path, healthy);
            assert_eq!(scan.entries.len(), 1);
            assert_eq!(scan.entries[0].key, healthy_key);
            let mut failed_paths = Vec::new();
            assert!(crate::connectors::codex::for_each_source_failure(error, |path, _| {
                failed_paths.push(path.to_path_buf());
            }));
            assert_eq!(failed_paths, vec![refused.clone()]);
            assert!(config.active_source_filter.take_deferred_sources().contains(&refused));
            assert_eq!(config.incomplete_remote_sources.load(Ordering::Relaxed), remote);
            assert!(config.source_ledger.contains_key(&refused_key));
            assert!(!config.source_ledger.contains_key(&healthy_key), "rebuild must disable reuse");
            assert!(scan.entries.iter().all(|entry| entry.key != refused_key));
        }
    }
    Ok(())
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn codex_prefix_full_receipt_checks_content_even_when_metadata_claim_matches() -> Result<()> {
    let temp = TempDir::new()?;
    let (path, ctx, config) = codex_prefix_fixture(&temp)?;
    File::open(&path)?.set_modified(SystemTime::now() - Duration::from_secs(7200))?;
    let (_, mut entries) = collect_ledger_scan(
        &crate::connectors::codex::CodexConnector::new(),
        &ctx,
        &config,
        "codex",
    )?;
    let entry = entries.pop().context("settled prefix receipt")?;
    let mut saved: serde_json::Value = serde_json::from_str(&entry.observation)?;
    assert_eq!(saved["codex_prefix"]["partial"], false);
    assert!(source_ledger_matches(
        &entry.observation,
        &discovered(&path, "codex")
    ));
    fs::write(&path, fs::read_to_string(&path)?.replace("first", "other"))?;
    // Supply current metadata deliberately: content verification must be an
    // independent gate, rather than another spelling of the stat comparator.
    saved["primary"] = source_file_observation(&path).unwrap();
    assert!(!source_ledger_matches(
        &saved.to_string(),
        &discovered(&path, "codex")
    ));
    Ok(())
}

#[cfg(unix)]
fn consume_codex_prefix_for_test(
    storage: &FrankenStorage,
    config: &StreamingProducerConfig,
    conversations: Vec<NormalizedConversation>,
    completion: crate::storage::sqlite::SourceIngestLedgerEntry,
) -> Result<NonWatchIngestOutcome> {
    let (tx, rx) = bounded(8);
    tx.send(IndexMessage::SourceComplete {
        connector_name: "codex",
        conversations,
        completion,
        byte_reservation: 0,
    })?;
    tx.send(IndexMessage::Done {
        connector_name: "codex",
        scan_ms: 1,
        is_discovered: true,
        scan_succeeded: true,
        active_source_skipped: false,
    })?;
    drop(tx);
    run_streaming_consumer(
        rx,
        1,
        storage,
        &config.data_dir,
        None,
        Arc::clone(&config.flow_limiter),
        &None,
        LexicalPopulationStrategy::DeferredAuthoritativeDbRebuild,
        Some(123_456),
        None,
    )
    .map(|(_, outcome)| outcome)
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn codex_prefix_consumer_replay_appends_without_duplicate_canonical_messages() -> Result<()> {
    let temp = TempDir::new()?;
    let (path, ctx, mut config) = codex_prefix_fixture(&temp)?;
    let mut writer = OpenOptions::new().append(true).open(&path)?;
    let storage = FrankenStorage::open(&config.data_dir.join("archive.db"))?;
    let connector = crate::connectors::codex::CodexConnector::new();
    for (pass, expected_inserted) in [(0, 1), (1, 1), (2, 0)] {
        if pass == 1 {
            writer.write_all(b"{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":\"second\"}}\n")?;
        }
        config.source_ledger = Arc::new(storage.source_ingest_ledger_entries()?);
        let (conversations, mut entries) = collect_ledger_scan(&connector, &ctx, &config, "codex")?;
        let outcome = consume_codex_prefix_for_test(
            &storage,
            &config,
            conversations,
            entries.pop().context("prefix completion")?,
        )?;
        assert_eq!(outcome.inserted_messages, expected_inserted);
        assert!(outcome.deferred_sources.contains(&path));
        assert!(outcome.scanned_connectors.is_empty());
        assert_eq!(storage.get_connector_last_scan_ts("codex")?, None);
    }
    let rows: i64 = storage
        .raw()
        .query_row_map("SELECT COUNT(*) FROM messages", &[], |row| row.get_typed(0))?;
    assert_eq!(rows, 2);
    Ok(())
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn codex_prefix_consumer_rejects_queued_mutation_without_erasing_prior_evidence() -> Result<()> {
    let temp = TempDir::new()?;
    let (path, ctx, config) = codex_prefix_fixture(&temp)?;
    let (conversations, mut entries) = collect_ledger_scan(
        &crate::connectors::codex::CodexConnector::new(),
        &ctx,
        &config,
        "codex",
    )?;
    let entry = entries.pop().context("prefix completion")?;
    let storage = FrankenStorage::open(&config.data_dir.join("archive.db"))?;
    let prior = invalidated_prefix_observation(&entry.observation)?.unwrap();
    storage.raw().execute_compat(
        "INSERT INTO meta(key, value) VALUES(?1, ?2)",
        &[
            ParamValue::from(entry.key.as_str()),
            ParamValue::from(prior.as_str()),
        ],
    )?;
    let saved = storage.source_ingest_ledger_entries()?;
    fs::write(&path, fs::read_to_string(&path)?.replace("first", "other"))?;
    let outcome = consume_codex_prefix_for_test(&storage, &config, conversations, entry)?;
    assert_eq!(outcome.inserted_messages, 0);
    assert!(outcome.scan_had_errors);
    assert!(outcome.deferred_sources.contains(&path));
    assert!(outcome.incomplete_remote_sources);
    assert_eq!(storage.get_connector_last_scan_ts("codex")?, None);
    assert_eq!(storage.source_ingest_ledger_entries()?, saved);
    Ok(())
}

#[test]
fn gh538_source_ledger_identity_preserves_existing_unicode_keys() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("session-雪.jsonl");
    fs::write(&path, b"retained source").unwrap();
    let source = discovered(&path, "claude_code");
    let mut root = ScanRoot::local(temp.path().to_path_buf());
    root.workspace_rewrites = vec![crate::sources::config::PathMapping::new(
        "/remote/workspace",
        "/local/雪",
    )];
    let ctx = ScanContext::with_roots(temp.path().to_path_buf(), vec![root], None);
    // Persisted source keys came from json! Values. Direct serialization of
    // structs can change object field order; keep the existing key contract.
    let mappings: Vec<_> = ctx
        .scan_roots
        .iter()
        .map(|root| serde_json::json!([root.path, root.workspace_rewrites]))
        .collect();
    let identity = serde_json::json!([
        source.provider_slug,
        source.scan_root,
        dunce::canonicalize(&path).unwrap(),
        source.origin.source_id,
        source.origin.kind,
        source.origin.host,
        mappings
    ]);
    let previous_key = format!(
        "source_ingest_v1:{}",
        blake3::hash(identity.to_string().as_bytes()).to_hex()
    );
    assert_eq!(source_ledger_key(&source, &ctx), Some(previous_key));
}

#[cfg(unix)]
#[test]
fn gh538_source_ledger_identity_rejects_unencodable_paths_without_panicking() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().unwrap();
    let first = temp
        .path()
        .join(OsString::from_vec(b"session-\xff.jsonl".to_vec()));
    let second = temp
        .path()
        .join(OsString::from_vec(b"session-\xfe.jsonl".to_vec()));
    fs::write(&first, b"retained first source").unwrap();
    assert_ne!(first, second);
    assert_eq!(first.to_string_lossy(), second.to_string_lossy());
    let mut ctx = ScanContext::with_roots(
        temp.path().to_path_buf(),
        vec![ScanRoot::local(temp.path().to_path_buf())],
        None,
    );
    for path in [&first, &second] {
        assert!(source_ledger_key(&discovered(path, "codex"), &ctx).is_none());
    }

    let ordinary = temp.path().join("ordinary.jsonl");
    fs::write(&ordinary, b"retained ordinary source").unwrap();
    let source = discovered(&ordinary, "codex");
    assert!(source_ledger_key(&source, &ctx).is_some());
    let mut bad_origin = source.clone();
    bad_origin.scan_root = first.clone();
    assert!(source_ledger_key(&bad_origin, &ctx).is_none());
    ctx.scan_roots.push(ScanRoot::local(second.clone()));
    assert!(source_ledger_key(&source, &ctx).is_none());
    ctx.scan_roots.pop();

    // The user-facing spelling can be Unicode while its canonical target is
    // not. It must not alias another native name or panic in json! either.
    let link = temp.path().join("ordinary-link.jsonl");
    symlink(&first, &link).unwrap();
    assert!(source_ledger_key(&discovered(&link, "codex"), &ctx).is_none());
    assert_eq!(fs::read(&first).unwrap(), b"retained first source");
    assert_eq!(fs::read(&ordinary).unwrap(), b"retained ordinary source");
}

#[cfg(unix)]
#[test]
fn gh538_unencodable_ledger_identity_keeps_real_source_parsing_without_ack() -> Result<()> {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let temp = TempDir::new()?;
    let path = temp.path().join("claude/projects/gh538/session.jsonl");
    fs::create_dir_all(path.parent().unwrap())?;
    fs::write(
        &path,
        concat!(
            "{\"type\":\"user\",\"sessionId\":\"gh538\",\"uuid\":\"message-1\",",
            "\"timestamp\":\"2026-08-01T10:00:00Z\",\"cwd\":\"/work/gh538\",",
            "\"message\":{\"role\":\"user\",\"content\":\"retained unencodable root\"}}\n",
        ),
    )?;
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir)?;
    let mut ctx = ScanContext::with_roots(data_dir.clone(), vec![ScanRoot::local(path)], None);
    let mut config = StreamingProducerConfig {
        source_ledger: Arc::new(HashMap::new()),
        incomplete_remote_sources: Arc::new(AtomicBool::new(false)),
        raw_source_exclusions: Arc::new(RawSourceExclusions::default()),
        flow_limiter: Arc::new(StreamingByteLimiter::new(STREAMING_MAX_BYTES_IN_FLIGHT)),
        data_dir,
        additional_scan_roots: Vec::new(),
        local_connector_roots: None,
        since_ts: None,
        local_since_ts_by_connector: Arc::new(HashMap::new()),
        progress: None,
        active_source_filter: Arc::new(ActiveSessionSourceFilter::default()),
    };
    let connector = crate::connectors::claude_code::ClaudeCodeConnector::new();
    let (control, mut entries) = collect_ledger_scan(&connector, &ctx, &config, "claude")?;
    assert_eq!(control.len(), 1);
    assert_eq!(entries.len(), 1);
    let entry = entries.pop().unwrap();
    config.source_ledger = Arc::new(HashMap::from([(entry.key, entry.observation)]));

    let unencodable_root = temp
        .path()
        .join(OsString::from_vec(b"workspace-\xff".to_vec()));
    fs::create_dir(&unencodable_root)?;
    ctx.scan_roots.push(ScanRoot::local(unencodable_root));
    let (parsed, entries) = collect_ledger_scan(&connector, &ctx, &config, "claude")?;
    assert_eq!(parsed.len(), 1, "unknown identity must not skip parsing");
    assert_eq!(
        serde_json::to_value(&parsed[0].messages)?,
        serde_json::to_value(&control[0].messages)?
    );
    assert!(
        entries.is_empty(),
        "unencodable identity cannot be certified"
    );
    Ok(())
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
            incomplete_remote_sources: Arc::new(AtomicBool::new(false)),
            raw_source_exclusions: Arc::new(RawSourceExclusions::default()),
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
        // Codex also fails the scan, so the run reports the source and keeps
        // its watermark (snapshot.rs:
        // sink_mutation_returns_failure_without_certifying_completion).
        fs::write(&source_path, format!("{transcript}\n"))?;
        let mutated = scan_with_durable_source_boundaries(
            &connector,
            &ctx,
            &config,
            &mut sender,
            |conversation| {
                fs::write(&source_path, format!("{transcript}\n\n"))?;
                Ok(Some(conversation))
            },
        );
        if name == "codex" {
            let error = mutated.expect_err("codex: a source changed after delivery fails the scan");
            assert!(
                error.chain().any(|cause| cause
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::Interrupted)),
                "{error:#}"
            );
        } else {
            mutated?;
        }
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
        scan_with_durable_source_boundaries(&connector, &ctx, &config, &mut sender, |_| {
            filtered_count += 1;
            Ok(None)
        })?;
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
#[serial_test::serial]
fn gh574_failed_local_scan_preserves_completed_ssh_source_reuse() -> Result<()> {
    let factories: Vec<_> = crate::connectors::get_connector_factories()
        .into_iter()
        .filter(|(name, _)| matches!(*name, "pi_agent" | "omp"))
        .collect();
    assert_eq!(factories.len(), 2);
    for (name, factory) in factories {
        for streaming in [false, true] {
            let temp = TempDir::new()?;
            let provider_dir = if name == "pi_agent" { ".pi" } else { ".omp" };
            let local_root = temp
                .path()
                .join("local")
                .join(provider_dir)
                .join("agent/sessions");
            let remote_root = temp
                .path()
                .join("remote")
                .join(provider_dir)
                .join("agent/sessions");
            let data_dir = temp.path().join("data");
            fs::create_dir_all(&data_dir)?;
            for (root, id) in [(&local_root, "local"), (&remote_root, "remote")] {
                fs::create_dir_all(root)?;
                let path = root.join("session.jsonl");
                let transcript = [
                    serde_json::json!({"type": "session", "version": 3, "id": id,
                        "timestamp": "2023-11-14T22:13:20Z", "cwd": "/work/gh574"}),
                    serde_json::json!({"type": "message", "timestamp": "2023-11-14T22:13:21Z",
                        "message": {"role": "user", "content": format!("{id} retained transcript")}}),
                ]
                .into_iter()
                .map(|row| row.to_string() + "\n")
                .collect::<String>();
                fs::write(&path, transcript)?;
                File::open(&path)?.set_modified(SystemTime::now() - Duration::from_secs(7200))?;
            }
            let remote = ScanRoot::remote(
                remote_root.clone(),
                Origin {
                    source_id: "gh574-mirror".to_owned(),
                    kind: SourceKind::Ssh,
                    host: Some("gh574-host".to_owned()),
                },
                None,
            );
            let local_ctx = ScanContext::with_roots(
                data_dir.clone(),
                vec![ScanRoot::local(local_root.clone())],
                None,
            );
            let remote_ctx = ScanContext::with_roots(remote_root, vec![remote.clone()], None);
            let connector = factory();
            let local_sources = connector.discover_source_files(&local_ctx)?;
            let remote_sources = connector.discover_source_files(&remote_ctx)?;
            assert_eq!(local_sources.len(), 1, "{name}: local fixture discovery");
            assert_eq!(remote_sources.len(), 1, "{name}: SSH fixture discovery");
            let local_key =
                source_ledger_key(&local_sources[0], &local_ctx).context("local source key")?;
            let remote_key =
                source_ledger_key(&remote_sources[0], &remote_ctx).context("SSH source key")?;
            let db_path = data_dir.join("archive.db");
            let storage = FrankenStorage::open(&db_path)?;
            // Refuse admission through the real prior-prefix validation path.
            // The producer reports this local ScanError before its SSH scan.
            storage.raw().execute_compat(
                "INSERT INTO meta(key, value) VALUES(?1, ?2)",
                &[ParamValue::from(local_key.as_str()), ParamValue::from("{")],
            )?;
            storage.set_connector_last_scan_ts(name, 123_456)?;
            let run = if streaming {
                run_streaming_index_with_connector_factories::<ConnectorFactory>
            } else {
                run_batch_index_with_connector_factories::<ConnectorFactory>
            };
            let mut identity = None;
            for pass in 0..2 {
                let progress = Arc::new(IndexingProgress::default());
                let opts = IndexOptions {
                    full: false,
                    force_rebuild: false,
                    watch: false,
                    watch_once_paths: None,
                    db_path: db_path.clone(),
                    data_dir: data_dir.clone(),
                    semantic: false,
                    build_hnsw: false,
                    embedder: "fastembed".to_owned(),
                    progress: Some(Arc::clone(&progress)),
                    watch_interval_secs: 30,
                };
                let outcome = run(
                    &storage,
                    None,
                    &opts,
                    None,
                    LexicalPopulationStrategy::DeferredAuthoritativeDbRebuild,
                    vec![remote.clone()],
                    Some(Arc::new(HashMap::from([(
                        name.to_owned(),
                        vec![local_root.clone()],
                    )]))),
                    vec![(name, factory)],
                    Some(456_789),
                    None,
                )?;
                assert!(
                    outcome.scan_had_errors,
                    "{name}, streaming={streaming}, pass={pass}"
                );
                assert!(outcome.scanned_connectors.is_empty());
                assert_eq!(storage.get_connector_last_scan_ts(name)?, Some(123_456));
                let saved = storage.source_ingest_ledger_entries()?;
                assert_eq!(saved.get(&local_key).map(String::as_str), Some("{"));
                assert!(
                    saved.contains_key(&remote_key),
                    "the earlier local error must not poison a healthy SSH completion"
                );
                let expected_parsed =
                    usize::from(pass == 0 || env!("CASS_SOURCE_INGEST_REUSE") != "true");
                let stats = progress.stats.lock().unwrap();
                assert_eq!(
                    (stats.total_conversations, stats.total_messages),
                    (expected_parsed, expected_parsed)
                );
                assert!(
                    stats
                        .connectors
                        .iter()
                        .any(|connector| connector.error.is_some())
                );
                drop(stats);
                let observed = storage.raw().query_row_map(
                    "SELECT c.id, a.slug, c.source_id, c.origin_host FROM conversations c \
                     JOIN agents a ON a.id = c.agent_id",
                    &[],
                    |row| {
                        Ok((
                            row.get_typed::<i64>(0)?,
                            row.get_typed::<String>(1)?,
                            row.get_typed::<String>(2)?,
                            row.get_typed::<String>(3)?,
                        ))
                    },
                )?;
                assert_eq!(observed.1, name);
                assert_eq!(observed.2, "gh574-mirror");
                assert_eq!(observed.3, "gh574-host");
                if let Some(expected) = &identity {
                    assert_eq!(&observed, expected);
                } else {
                    identity = Some(observed);
                }
                assert_eq!(count_total_messages_exact(&storage)?, 1);
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn gh574_failed_publication_still_withholds_later_source_completion() -> Result<()> {
    let temp = TempDir::new()?;
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir)?;
    let config = diagnostic_ledger_config(data_dir.clone());
    let storage = FrankenStorage::open(&data_dir.join("archive.db"))?;
    storage.set_connector_last_scan_ts("codex", 123_456)?;
    let (failed, failed_receipt) =
        canonical_refusal_packet(temp.path(), &config, "publication-failed", 1, true)?;
    let (healthy, healthy_receipt) =
        canonical_refusal_packet(temp.path(), &config, "publication-later", 1, true)?;
    let failed_path = failed.source_path.clone();
    let failed_key = failed_receipt.key.clone();
    let healthy_key = healthy_receipt.key.clone();
    let invalidated = invalidated_prefix_observation(&healthy_receipt.observation)?
        .context("healthy fixture prefix proof")?;
    // Change a parsed source before its queued packet can publish. This is an
    // ingestion failure, so a later completion must retain the conservative
    // latch: an earlier deferred Batch could belong to that later source.
    fs::write(
        &failed_path,
        fs::read_to_string(&failed_path)?.replace("message 0", "changed 0"),
    )?;
    let (tx, rx) = bounded(8);
    send_canonical_refusal_packet(&tx, &config.flow_limiter, failed, failed_receipt)?;
    send_canonical_refusal_packet(&tx, &config.flow_limiter, healthy, healthy_receipt)?;
    send_canonical_refusal_done(&tx)?;
    drop(tx);
    let (_, outcome) = run_streaming_consumer(
        rx,
        1,
        &storage,
        &data_dir,
        None,
        Arc::clone(&config.flow_limiter),
        &None,
        LexicalPopulationStrategy::DeferredAuthoritativeDbRebuild,
        Some(456_789),
        None,
    )?;
    assert_eq!(config.flow_limiter.bytes_in_flight(), 0);
    assert!(outcome.scan_had_errors);
    assert!(outcome.incomplete_remote_sources);
    assert_eq!(outcome.deferred_sources, BTreeSet::from([failed_path]));
    assert!(outcome.scanned_connectors.is_empty());
    assert_eq!(storage.get_connector_last_scan_ts("codex")?, Some(123_456));
    assert_eq!(outcome.inserted_messages, 1);
    assert_eq!(count_total_messages_exact(&storage)?, 1);
    let saved = storage.source_ingest_ledger_entries()?;
    assert!(!saved.contains_key(&failed_key));
    assert_eq!(
        saved.get(&healthy_key),
        Some(&invalidated),
        "failed publication must not certify a later completion"
    );
    Ok(())
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
        incomplete_remote_sources: Arc::new(AtomicBool::new(false)),
        raw_source_exclusions: Arc::new(RawSourceExclusions::default()),
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

struct DiagnosticLedgerScan {
    conversations: Vec<NormalizedConversation>,
    entries: Vec<crate::storage::sqlite::SourceIngestLedgerEntry>,
    reused_sources: Vec<PathBuf>,
    report: ConnectorIngestReport,
    scan_error: Option<anyhow::Error>,
}

fn diagnostic_ledger_config(data_dir: PathBuf) -> StreamingProducerConfig {
    StreamingProducerConfig {
        source_ledger: Arc::new(HashMap::new()),
        incomplete_remote_sources: Arc::new(AtomicBool::new(false)),
        raw_source_exclusions: Arc::new(RawSourceExclusions::default()),
        flow_limiter: Arc::new(StreamingByteLimiter::new(STREAMING_MAX_BYTES_IN_FLIGHT)),
        data_dir,
        additional_scan_roots: Vec::new(),
        local_connector_roots: None,
        since_ts: None,
        local_since_ts_by_connector: Arc::new(HashMap::new()),
        progress: None,
        active_source_filter: Arc::new(ActiveSessionSourceFilter::default()),
    }
}

/// Exercise the production capture, source-admission, and diagnostic paths
/// together. The callback injects a real file change after preparse discovery,
/// without replacing the connector, ledger matcher, or source-completion hooks.
fn collect_diagnostic_ledger_scan(
    connector: &(dyn Connector + Send),
    ctx: &ScanContext,
    config: &StreamingProducerConfig,
    name: &'static str,
    before_scan: impl FnOnce() -> Result<()>,
) -> Result<DiagnosticLedgerScan> {
    let (mut diagnostics, active_skipped) = capture_connector_sources_before_parse(
        connector,
        ctx,
        &config.data_dir,
        name,
        &ctx.scan_roots,
        ctx.since_ts,
        config.active_source_filter.as_ref(),
        &config.raw_source_exclusions,
    );
    assert!(!active_skipped);
    before_scan()?;
    let limiter = Arc::clone(&config.flow_limiter);
    let (tx, rx) = bounded(8);
    let mut sender = StreamingBatchSender::new(&tx, Arc::clone(&limiter), name, true);
    let scan_error =
        scan_with_durable_source_boundaries(connector, ctx, config, &mut sender, |mut conv| {
            diagnostics.observe_conversation(&mut conv);
            Ok(Some(conv))
        })
        .err();
    if let Some(error) = &scan_error {
        let fallback = ctx
            .scan_roots
            .first()
            .map_or(ctx.data_dir.as_path(), |root| root.path.as_path());
        diagnostics.observe_connector_scan_error(fallback, error);
    }
    sender.flush()?;
    let reused_sources = std::mem::take(&mut sender.reused_sources);
    diagnostics.observe_reused_sources(reused_sources.iter().cloned());
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
    Ok(DiagnosticLedgerScan {
        conversations,
        entries,
        reused_sources,
        report: diagnostics.finish(),
        scan_error,
    })
}

#[test]
fn gh555_real_source_reuse_skips_repeated_jsonl_diagnostics() -> Result<()> {
    use crate::connector_ingest_diagnostics::IngestFailureKind;

    let temp = TempDir::new()?;
    let path = temp.path().join("claude/projects/gh555/session.jsonl");
    fs::create_dir_all(path.parent().unwrap())?;
    // The real parser preserves this message and certifies the source despite
    // the quarantined line. An eager diagnostic reread necessarily finds that
    // line again, making duplicate inspection observable without timing tests.
    let transcript = concat!(
        "not-json\n",
        "{\"type\":\"user\",\"sessionId\":\"gh555\",\"uuid\":\"message-1\",",
        "\"timestamp\":\"2026-08-01T10:00:00Z\",\"cwd\":\"/work/gh555\",",
        "\"message\":{\"role\":\"user\",\"content\":\"retained transcript\"}}\n",
    );
    fs::write(&path, transcript)?;
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir)?;
    let ctx = ScanContext::with_roots(data_dir.clone(), vec![ScanRoot::local(path.clone())], None);
    let mut config = diagnostic_ledger_config(data_dir);
    let connector = crate::connectors::claude_code::ClaudeCodeConnector::new();
    let first = collect_diagnostic_ledger_scan(&connector, &ctx, &config, "claude", || Ok(()))?;
    assert!(first.scan_error.is_none());
    assert_eq!(first.conversations.len(), 1);
    assert_eq!(first.entries.len(), 1);
    assert_eq!(first.report.summary.partially_indexed, 1);
    assert_eq!(first.report.diagnostics.len(), 1);
    assert_eq!(
        first.report.diagnostics[0].failure_kind,
        IngestFailureKind::MalformedJsonLine
    );
    let messages = serde_json::to_value(&first.conversations[0].messages)?;
    config.source_ledger = Arc::new(
        first
            .entries
            .into_iter()
            .map(|entry| (entry.key, entry.observation))
            .collect(),
    );

    let next = collect_diagnostic_ledger_scan(&connector, &ctx, &config, "claude", || Ok(()))?;
    assert!(next.scan_error.is_none());
    if env!("CASS_SOURCE_INGEST_REUSE") == "true" {
        assert_eq!(next.reused_sources, vec![path.clone()]);
        assert!(next.conversations.is_empty());
        assert!(next.entries.is_empty());
        assert_eq!(next.report.summary.indexed, 1);
        assert!(
            next.report.diagnostics.is_empty(),
            "an actually reused source must not be opened for duplicate JSONL diagnostics"
        );
    } else {
        assert!(next.reused_sources.is_empty());
        assert_eq!(next.conversations.len(), 1);
        assert_eq!(next.report.summary.partially_indexed, 1);
        assert_eq!(next.report.diagnostics.len(), 1);
    }

    // Matching file identity is insufficient after producer-contract drift:
    // the real admission hook must reparse and diagnose the source again.
    for observation in Arc::make_mut(&mut config.source_ledger).values_mut() {
        let mut saved: serde_json::Value = serde_json::from_str(observation)?;
        saved["producer_contract"] = serde_json::json!("gh555-obsolete-producer");
        *observation = saved.to_string();
    }
    let replay = collect_diagnostic_ledger_scan(&connector, &ctx, &config, "claude", || Ok(()))?;
    assert!(replay.scan_error.is_none());
    assert!(replay.reused_sources.is_empty());
    assert_eq!(replay.conversations.len(), 1);
    assert_eq!(replay.entries.len(), 1);
    assert_eq!(
        serde_json::to_value(&replay.conversations[0].messages)?,
        messages
    );
    assert_eq!(replay.report.summary.partially_indexed, 1);
    assert_eq!(replay.report.diagnostics.len(), 1);
    assert_eq!(
        replay.report.diagnostics[0].failure_kind,
        IngestFailureKind::MalformedJsonLine
    );
    assert_eq!(fs::read_to_string(path)?, transcript);
    Ok(())
}

#[test]
fn gh555_primary_changed_after_capture_still_receives_diagnostics() -> Result<()> {
    use crate::connector_ingest_diagnostics::IngestFailureKind;

    let temp = TempDir::new()?;
    let path = temp.path().join("claude/projects/gh555/session.jsonl");
    fs::create_dir_all(path.parent().unwrap())?;
    let transcript = concat!(
        "{\"type\":\"user\",\"sessionId\":\"gh555\",\"uuid\":\"message-1\",",
        "\"timestamp\":\"2026-08-01T10:00:00Z\",\"cwd\":\"/work/gh555\",",
        "\"message\":{\"role\":\"user\",\"content\":\"retained transcript\"}}\n",
    );
    fs::write(&path, transcript)?;
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir)?;
    let ctx = ScanContext::with_roots(data_dir.clone(), vec![ScanRoot::local(path.clone())], None);
    let mut config = diagnostic_ledger_config(data_dir);
    let connector = crate::connectors::claude_code::ClaudeCodeConnector::new();
    let first = collect_diagnostic_ledger_scan(&connector, &ctx, &config, "claude", || Ok(()))?;
    assert!(first.scan_error.is_none());
    assert_eq!(first.entries.len(), 1);
    assert!(first.report.diagnostics.is_empty());
    config.source_ledger = Arc::new(
        first
            .entries
            .into_iter()
            .map(|entry| (entry.key, entry.observation))
            .collect(),
    );

    let changed = format!("not-json\n{transcript}");
    let next = collect_diagnostic_ledger_scan(&connector, &ctx, &config, "claude", || {
        fs::write(&path, &changed)?;
        Ok(())
    })?;
    assert!(next.scan_error.is_none());
    assert!(next.reused_sources.is_empty());
    assert_eq!(next.conversations.len(), 1);
    assert_eq!(next.entries.len(), 1);
    assert_eq!(
        next.conversations[0].messages[0].content,
        "retained transcript"
    );
    assert_eq!(next.report.summary.partially_indexed, 1);
    assert_eq!(next.report.diagnostics.len(), 1);
    assert_eq!(
        next.report.diagnostics[0].failure_kind,
        IngestFailureKind::MalformedJsonLine
    );
    assert_eq!(next.report.diagnostics[0].line_or_row, Some(1));
    assert_eq!(fs::read_to_string(path)?, changed);
    Ok(())
}

#[test]
fn gh555_failed_sibling_preserves_reuse_and_later_source_diagnostics() -> Result<()> {
    use crate::connector_ingest_diagnostics::IngestFailureKind;

    let temp = TempDir::new()?;
    let paths = [
        temp.path().join("rollout-a-unchanged.jsonl"),
        temp.path().join("rollout-b-failing.jsonl"),
        temp.path().join("rollout-c-changed.jsonl"),
    ];
    let message = |text: &str| {
        format!(
            "{}\n",
            serde_json::json!({
                "type": "response_item",
                "payload": {"role": "user", "content": text}
            })
        )
    };
    fs::write(&paths[0], format!("not-json\n{}", message("retained")))?;
    fs::write(&paths[1], message("before failure"))?;
    fs::write(&paths[2], message("before change"))?;
    // Reuse requires settled EOF. A live prefix deliberately never grants
    // whole-source reuse, even when its current bytes have not changed.
    for path in &paths {
        File::open(path)?.set_modified(SystemTime::now() - Duration::from_secs(7200))?;
    }
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir)?;
    let ctx = ScanContext::with_roots(
        data_dir.clone(),
        paths.iter().cloned().map(ScanRoot::local).collect(),
        None,
    );
    let mut config = diagnostic_ledger_config(data_dir);
    let connector = crate::connectors::codex::CodexConnector::new();
    let first = collect_diagnostic_ledger_scan(&connector, &ctx, &config, "codex", || Ok(()))?;
    assert!(first.scan_error.is_none());
    assert_eq!(first.conversations.len(), 3);
    assert_eq!(first.entries.len(), 3);
    assert_eq!(first.report.diagnostics.len(), 1);
    config.source_ledger = Arc::new(
        first
            .entries
            .into_iter()
            .map(|entry| (entry.key, entry.observation))
            .collect(),
    );

    // Truncating a receipt-protected source now fails its prior byte proof
    // before parsing. The next source retains its indexed prefix and appends
    // a new contribution; only the rejected sibling withholds its receipt.
    fs::write(&paths[1], b"\xff\n")?;
    OpenOptions::new().append(true).open(&paths[2])?
        .write_all(message("new contribution").as_bytes())?;
    let next = collect_diagnostic_ledger_scan(&connector, &ctx, &config, "codex", || Ok(()))?;
    assert!(next.scan_error.is_some());
    let reuse_enabled = env!("CASS_SOURCE_INGEST_REUSE") == "true";
    assert_eq!(next.conversations.len(), if reuse_enabled { 1 } else { 2 });
    assert_eq!(next.entries.len(), next.conversations.len());
    assert!(
        next.conversations.iter().any(|conversation| {
            conversation.source_path == paths[2]
                && conversation.messages.iter().any(|message| message.content == "new contribution")
        }),
        "a failed source must not starve the later admitted rollout"
    );
    assert_eq!(next.report.summary.total(), 3);
    assert_eq!(next.report.summary.with_content(), 2);
    assert_eq!(next.report.summary.skipped, 1);
    let failed_path = paths[1].display().to_string();
    assert!(next.report.diagnostics.iter().any(|diagnostic| {
        diagnostic.source_path == failed_path
            && diagnostic.failure_kind == IngestFailureKind::SourceChanged
    }));
    if reuse_enabled {
        assert_eq!(next.reused_sources, vec![paths[0].clone()]);
        assert!(
            next.report
                .diagnostics
                .iter()
                .all(|diagnostic| diagnostic.source_path == failed_path),
            "the failed sibling must not trigger a diagnostic reread of a reused rollout"
        );
    } else {
        assert!(next.reused_sources.is_empty());
    }
    assert_eq!(fs::read(&paths[1])?, b"\xff\n");
    Ok(())
}

#[cfg(unix)]
struct CanonicalRefusalTestEnv {
    limit: Option<std::ffi::OsString>,
    cadence: Option<std::ffi::OsString>,
}

#[cfg(unix)]
impl CanonicalRefusalTestEnv {
    fn new(stop_after_first_success: bool) -> Self {
        let guard = Self {
            limit: std::env::var_os("CASS_INDEX_MAX_SOURCE_COMMITS"),
            cadence: std::env::var_os("CASS_STREAMING_CONSUMER_COMMIT_SECS"),
        };
        // SAFETY: every caller is a serial test, and Drop restores both keys.
        unsafe {
            if stop_after_first_success {
                std::env::set_var("CASS_INDEX_MAX_SOURCE_COMMITS", "1");
            } else {
                std::env::remove_var("CASS_INDEX_MAX_SOURCE_COMMITS");
            }
            std::env::set_var("CASS_STREAMING_CONSUMER_COMMIT_SECS", "3600");
        }
        guard
    }
}

#[cfg(unix)]
impl Drop for CanonicalRefusalTestEnv {
    fn drop(&mut self) {
        // SAFETY: this serial test guard restores only its own two changes.
        unsafe {
            for (key, previous) in [
                ("CASS_INDEX_MAX_SOURCE_COMMITS", &self.limit),
                ("CASS_STREAMING_CONSUMER_COMMIT_SECS", &self.cadence),
            ] {
                if let Some(value) = previous {
                    std::env::set_var(key, value);
                } else {
                    std::env::remove_var(key);
                }
            }
        }
    }
}

#[cfg(unix)]
fn canonical_refusal_packet(
    root: &Path,
    config: &StreamingProducerConfig,
    name: &str,
    messages: usize,
    remote: bool,
) -> Result<(
    NormalizedConversation,
    crate::storage::sqlite::SourceIngestLedgerEntry,
)> {
    canonical_refusal_packet_for_provider(
        root,
        config,
        name,
        messages,
        remote,
        crate::connectors::codex::StablePrefixProvider::Codex,
    )
}

#[cfg(unix)]
fn canonical_refusal_packet_for_provider(
    root: &Path,
    config: &StreamingProducerConfig,
    name: &str,
    messages: usize,
    remote: bool,
    provider: crate::connectors::codex::StablePrefixProvider,
) -> Result<(
    NormalizedConversation,
    crate::storage::sqlite::SourceIngestLedgerEntry,
)> {
    use crate::connectors::codex::StablePrefixProvider;

    let source = root.join(format!("rollout-{name}.jsonl"));
    let mut bytes = String::new();
    for idx in 0..messages {
        let timestamp = format!("2026-11-23T12:00:{idx:02}Z");
        let content = format!("canonical isolation fixture {name} message {idx}");
        let record = match provider {
            StablePrefixProvider::Codex => serde_json::json!({
                "type": "response_item",
                "timestamp": timestamp,
                "payload": {"type": "message", "role": "user", "content": content}
            }),
            StablePrefixProvider::ClaudeCode => serde_json::json!({
                "type": "user",
                "timestamp": timestamp,
                "sessionId": name,
                "uuid": format!("{name}-{idx}"),
                "message": {"role": "user", "content": content}
            }),
            StablePrefixProvider::PiAgent | StablePrefixProvider::Omp => {
                unreachable!("this fixture exercises Codex and Claude canonical preflight")
            }
        };
        bytes.push_str(&record.to_string());
        bytes.push('\n');
    }
    fs::write(&source, bytes)?;
    File::open(&source)?.set_modified(SystemTime::now() - Duration::from_secs(7200))?;
    let scan_root = if remote {
        ScanRoot::remote(source, Origin::remote("canonical-refusal-fixture"), None)
    } else {
        ScanRoot::local(source)
    };
    let ctx = ScanContext::with_roots(config.data_dir.clone(), vec![scan_root], None);
    let (connector, connector_name): (Box<dyn Connector>, &'static str) = match provider {
        StablePrefixProvider::Codex => (
            Box::new(crate::connectors::codex::CodexConnector::new()),
            "codex",
        ),
        StablePrefixProvider::ClaudeCode => (
            Box::new(crate::connectors::claude_code::ClaudeCodeConnector::new()),
            "claude",
        ),
        StablePrefixProvider::PiAgent | StablePrefixProvider::Omp => {
            unreachable!("this fixture exercises Codex and Claude canonical preflight")
        }
    };
    let (mut packets, mut entries) =
        collect_ledger_scan(connector.as_ref(), &ctx, config, connector_name)?;
    assert_eq!(packets.len(), 1);
    assert_eq!(entries.len(), 1);
    let packet = packets.remove(0);
    let entry = entries.remove(0);
    let observation: serde_json::Value = serde_json::from_str(&entry.observation)?;
    assert_eq!(packet.agent_slug, provider.slug());
    assert_eq!(observation[provider.observation_key()]["partial"], false);
    Ok((packet, entry))
}

#[cfg(unix)]
fn send_canonical_refusal_packet(
    tx: &Sender<IndexMessage>,
    limiter: &StreamingByteLimiter,
    packet: NormalizedConversation,
    completion: crate::storage::sqlite::SourceIngestLedgerEntry,
) -> Result<()> {
    let connector_name = match packet.agent_slug.as_str() {
        "codex" => "codex",
        "claude_code" => "claude",
        _ => anyhow::bail!("canonical refusal fixture has an unsupported parser"),
    };
    let byte_reservation = limiter.acquire(1024)?;
    tx.send(IndexMessage::SourceComplete {
        connector_name,
        conversations: vec![packet],
        completion,
        byte_reservation,
    })?;
    Ok(())
}

#[cfg(unix)]
fn send_canonical_refusal_done(tx: &Sender<IndexMessage>) -> Result<()> {
    tx.send(IndexMessage::Done {
        connector_name: "codex",
        scan_ms: 1,
        is_discovered: true,
        scan_succeeded: true,
        active_source_skipped: false,
    })?;
    Ok(())
}

#[cfg(unix)]
fn assert_canonical_refusal_repair_pending(data_dir: &Path) -> Result<()> {
    let state = load_lexical_rebuild_state(&index_dir(data_dir)?)?
        .context("canonical refusal lost its durable lexical repair debt")?;
    assert!(state.is_incomplete());
    assert_eq!(
        state.effective_execution_mode(),
        LexicalRebuildExecutionMode::CanonicalMetadataRepair
    );
    Ok(())
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn codex_canonical_refusal_startup_preserves_healthy_sibling_receipts() -> Result<()> {
    canonical_refusal_startup_preserves_healthy_sibling_receipts(
        crate::connectors::codex::StablePrefixProvider::Codex,
    )
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn claude_canonical_refusal_startup_preserves_healthy_sibling_receipts() -> Result<()> {
    canonical_refusal_startup_preserves_healthy_sibling_receipts(
        crate::connectors::codex::StablePrefixProvider::ClaudeCode,
    )
}

#[cfg(unix)]
fn canonical_refusal_startup_preserves_healthy_sibling_receipts(
    provider: crate::connectors::codex::StablePrefixProvider,
) -> Result<()> {
    let connector_name = match provider {
        crate::connectors::codex::StablePrefixProvider::Codex => "codex",
        crate::connectors::codex::StablePrefixProvider::ClaudeCode => "claude",
        crate::connectors::codex::StablePrefixProvider::PiAgent
        | crate::connectors::codex::StablePrefixProvider::Omp => {
            unreachable!("this fixture exercises Codex and Claude canonical preflight")
        }
    };
    let _env = CanonicalRefusalTestEnv::new(false);
    for remote in [false, true] {
        for refused_first in [false, true] {
            let temp = TempDir::new()?;
            let data_dir = temp.path().join("data");
            fs::create_dir(&data_dir)?;
            let config = diagnostic_ledger_config(data_dir.clone());
            let db_path = data_dir.join("archive.db");
            let storage = FrankenStorage::open(&db_path)?;
            let mut index = TantivyIndex::open_or_create(&index_dir(&data_dir)?)?;
            let before = canonical_refusal_packet_for_provider(
                temp.path(),
                &config,
                "a-healthy",
                1,
                remote,
                provider,
            )?;
            let refused = canonical_refusal_packet_for_provider(
                temp.path(),
                &config,
                "b-refused",
                1,
                remote,
                provider,
            )?;
            let after = canonical_refusal_packet_for_provider(
                temp.path(),
                &config,
                "c-healthy",
                1,
                remote,
                provider,
            )?;
            let mut legacy = refused.0.clone();
            legacy.metadata["cass"]
                .as_object_mut()
                .unwrap()
                .remove(provider.metadata_key());
            legacy.messages[0].content = "retained divergent canonical history".to_owned();
            persist::persist_conversation(&storage, &mut index, &legacy)?;
            index.commit()?;
            assert_eq!(index.doc_count()?, 1);
            let refused_proof = invalidated_prefix_observation(&refused.1.observation)?.unwrap();
            let refused_path = refused.0.source_path.clone();
            let refused_key = refused.1.key.clone();
            let healthy_receipts = [
                (before.1.key.clone(), before.1.observation.clone()),
                (after.1.key.clone(), after.1.observation.clone()),
            ];
            let (tx, rx) = bounded(8);
            let packets = if refused_first {
                vec![refused, before, after]
            } else {
                vec![before, refused, after]
            };
            for (packet, receipt) in packets {
                send_canonical_refusal_packet(&tx, &config.flow_limiter, packet, receipt)?;
            }
            tx.send(IndexMessage::Done {
                connector_name,
                scan_ms: 1,
                is_discovered: true,
                scan_succeeded: true,
                active_source_skipped: false,
            })?;
            drop(tx);
            let progress = Arc::new(IndexingProgress::default());
            let (_, outcome) = run_streaming_consumer(
                rx,
                1,
                &storage,
                &data_dir,
                Some(&mut index),
                Arc::clone(&config.flow_limiter),
                &Some(Arc::clone(&progress)),
                LexicalPopulationStrategy::IncrementalInline,
                Some(123_456),
                None,
            )?;
            assert_eq!(config.flow_limiter.bytes_in_flight(), 0);
            assert_eq!(outcome.inserted_conversations, 2);
            assert_eq!(outcome.inserted_messages, 2);
            assert!(outcome.lexical_update_deferred);
            assert!(outcome.scan_had_errors);
            assert!(outcome.incomplete_remote_sources);
            assert_eq!(outcome.deferred_sources, BTreeSet::from([refused_path]));
            assert!(outcome.scanned_connectors.is_empty());
            assert_eq!(storage.get_connector_last_scan_ts(connector_name)?, None);
            let saved = storage.source_ingest_ledger_entries()?;
            assert_eq!(saved.get(&refused_key), Some(&refused_proof));
            for (key, observation) in healthy_receipts {
                assert_eq!(saved.get(&key), Some(&observation));
            }
            let stats = progress.stats.lock().unwrap();
            assert_eq!(
                stats.deferred_active_sources, 0,
                "canonical refusal is not an active writer"
            );
            assert!(stats.connectors.iter().any(|connector| {
                connector
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("canonical source deferred"))
            }));
            drop(stats);
            assert_canonical_refusal_repair_pending(&data_dir)?;
            assert_eq!(
                index.doc_count()?,
                1,
                "no abandoned staged documents may publish"
            );

            // Mirror run_index's existing finalization boundary: drop the owner,
            // then rebuild from actual canonical rows rather than lost counts.
            drop(index);
            rebuild_tantivy_from_db_deferred_startup(&db_path, &data_dir, 3, None)?;
            let index = TantivyIndex::open_or_create(&index_dir(&data_dir)?)?;
            assert_eq!(index.doc_count()?, 3);
            assert_eq!(count_total_messages_exact(&storage)?, 3);
            assert_eq!(storage.source_ingest_ledger_entries()?, saved);
        }
    }
    Ok(())
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn codex_canonical_refusal_stop_counts_only_the_later_successful_source() -> Result<()> {
    let _env = CanonicalRefusalTestEnv::new(true);
    let temp = TempDir::new()?;
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir)?;
    let config = diagnostic_ledger_config(data_dir.clone());
    let storage = FrankenStorage::open(&data_dir.join("archive.db"))?;
    let mut index = TantivyIndex::open_or_create(&index_dir(&data_dir)?)?;
    let (refused, receipt) =
        canonical_refusal_packet(temp.path(), &config, "refused-stop", 1, false)?;
    let (healthy, healthy_receipt) =
        canonical_refusal_packet(temp.path(), &config, "healthy-stop", 1, false)?;
    let mut legacy = refused.clone();
    legacy.metadata["cass"].as_object_mut().unwrap().remove("codex_stable_prefix");
    legacy.messages[0].content = "retained legacy variant before interruption".to_owned();
    persist::persist_conversation(&storage, &mut index, &legacy)?;
    index.commit()?;
    let invalidated = invalidated_prefix_observation(&receipt.observation)?.unwrap();
    let refused_key = receipt.key.clone();
    let healthy_key = healthy_receipt.key.clone();
    let healthy_observation = healthy_receipt.observation.clone();
    let (tx, rx) = bounded(8);
    send_canonical_refusal_packet(&tx, &config.flow_limiter, refused, receipt)?;
    send_canonical_refusal_packet(&tx, &config.flow_limiter, healthy, healthy_receipt)?;
    send_canonical_refusal_done(&tx)?;
    drop(tx);
    let progress = Arc::new(IndexingProgress::default());
    let error = run_streaming_consumer(
        rx, 1, &storage, &data_dir, Some(&mut index),
        Arc::clone(&config.flow_limiter), &Some(progress),
        LexicalPopulationStrategy::IncrementalInline, Some(123_456), None,
    ).err().context("source-commit stop did not interrupt")?;
    assert!(error.downcast_ref::<IndexInterrupted>().is_some());
    assert_eq!(config.flow_limiter.bytes_in_flight(), 0);
    let saved = storage.source_ingest_ledger_entries()?;
    assert_eq!(saved.get(&refused_key), Some(&invalidated));
    assert_eq!(saved.get(&healthy_key), Some(&healthy_observation),
        "the refused attempt must not consume the one-successful-source limit");
    assert_canonical_refusal_repair_pending(&data_dir)?;
    assert_eq!(index.doc_count()?, 1, "interruption must not commit the detached writer");
    Ok(())
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn codex_canonical_refusal_late_slice_keeps_committed_rows_and_repair_debt() -> Result<()> {
    let temp = TempDir::new()?;
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir)?;
    let config = diagnostic_ledger_config(data_dir.clone());
    let db_path = data_dir.join("archive.db");
    let storage = FrankenStorage::open(&db_path)?;
    let mut index = TantivyIndex::open_or_create(&index_dir(&data_dir)?)?;
    let (packet, receipt) = canonical_refusal_packet(temp.path(), &config, "late-slice", 4, false)?;
    let mut legacy = packet.clone();
    legacy.metadata["cass"].as_object_mut().unwrap().remove("codex_stable_prefix");
    legacy.messages = vec![legacy.messages[0].clone(), legacy.messages[3].clone()];
    legacy.messages[1].content = "canonical conflict in a later slice".to_owned();
    persist::persist_conversation(&storage, &mut index, &legacy)?;
    index.commit()?;
    let invalidated = invalidated_prefix_observation(&receipt.observation)?.unwrap();
    storage.raw().execute_compat(
        "INSERT INTO meta(key, value) VALUES(?1, ?2)",
        &[ParamValue::from(receipt.key.as_str()), ParamValue::from(invalidated.as_str())],
    )?;
    let error = persist::persist_conversations_batched_sliced(
        &storage, Some(&mut index), std::slice::from_ref(&packet),
        LexicalPopulationStrategy::IncrementalInline, true, false, Some(&data_dir),
        persist::PersistHeartbeat::NONE, Some(&receipt),
        persist::PersistSliceLimits { messages: 2, content_bytes: usize::MAX },
    ).err().context("late divergent slice was accepted")?;
    let refusal = source_exclusive_codex_canonical_refusal(&error, "codex", std::slice::from_ref(&packet))
        .context("late conflict lost its typed source identity")?;
    assert_eq!(count_total_messages_exact(&storage)?, 3,
        "the first slice committed idx 1; the failing second transaction rolled back idx 2");
    let outcome = defer_startup_codex_canonical_refusal(&storage, &data_dir, refusal)?;
    assert!(outcome.lexical_update_deferred);
    assert!(outcome.scan_had_errors);
    assert_eq!(outcome.deferred_sources, BTreeSet::from([packet.source_path]));
    assert_eq!(storage.source_ingest_ledger_entries()?.get(&receipt.key), Some(&invalidated));
    assert_canonical_refusal_repair_pending(&data_dir)?;
    drop(index);
    rebuild_tantivy_from_db_deferred_startup(&db_path, &data_dir, 1, None)?;
    let index = TantivyIndex::open_or_create(&index_dir(&data_dir)?)?;
    assert_eq!(index.doc_count()?, 3, "authoritative repair retains the actual earlier commit");
    Ok(())
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn codex_canonical_refusal_marker_io_failure_stays_terminal_and_releases_bytes() -> Result<()> {
    let temp = TempDir::new()?;
    let data_dir = temp.path().join("data");
    fs::create_dir(&data_dir)?;
    let config = diagnostic_ledger_config(data_dir.clone());
    let storage = FrankenStorage::open(&data_dir.join("archive.db"))?;
    let (packet, receipt) = canonical_refusal_packet(temp.path(), &config, "marker-io", 1, false)?;
    let mut legacy = persist::map_to_internal(&packet);
    legacy.metadata_json["cass"].as_object_mut().unwrap().remove("codex_stable_prefix");
    legacy.messages[0].content = "retained canonical marker failure variant".to_owned();
    let agent = storage.ensure_agent(&crate::model::types::Agent {
        id: None, slug: "codex".to_owned(), name: "Codex".to_owned(),
        version: None, kind: crate::model::types::AgentKind::Cli,
    })?;
    storage.insert_conversation_tree_with_analytics(agent, None, &legacy, false)?;
    // A directory at the file destination makes the atomic checkpoint rename
    // fail without relying on permission behavior under privileged test users.
    fs::create_dir_all(lexical_rebuild_state_path(&index_dir(&data_dir)?))?;
    let (tx, rx) = bounded(8);
    send_canonical_refusal_packet(&tx, &config.flow_limiter, packet, receipt)?;
    send_canonical_refusal_done(&tx)?;
    drop(tx);
    let error = run_streaming_consumer(
        rx, 1, &storage, &data_dir, None, Arc::clone(&config.flow_limiter), &None,
        LexicalPopulationStrategy::DeferredAuthoritativeDbRebuild, Some(123_456), None,
    ).err().context("checkpoint I/O failure was swallowed")?;
    assert!(error.downcast_ref::<crate::storage::sqlite::CodexCanonicalRefusal>().is_none());
    assert!(error.downcast_ref::<std::io::Error>().is_some());
    assert_eq!(config.flow_limiter.bytes_in_flight(), 0);
    assert_eq!(storage.get_connector_last_scan_ts("codex")?, None);
    Ok(())
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn codex_canonical_refusal_classification_is_typed_and_source_exclusive() -> Result<()> {
    use crate::storage::sqlite::{CodexCanonicalRefusal, CodexCanonicalRefusalReason};
    let temp = TempDir::new()?;
    let (_, ctx, config) = codex_prefix_fixture(&temp)?;
    let (packets, _) = collect_ledger_scan(
        &crate::connectors::codex::CodexConnector::new(), &ctx, &config, "codex",
    )?;
    let packet = &packets[0];
    let refusal = || anyhow::Error::new(CodexCanonicalRefusal {
        provider: crate::connectors::codex::StablePrefixProvider::Codex,
        source_path: packet.source_path.clone(),
        external_id: packet.external_id.clone(),
        conversation_id: 42,
        reason: CodexCanonicalRefusalReason::ProjectionMismatch(0),
    }).context("canonical transaction");
    let error = refusal();
    assert!(source_exclusive_codex_canonical_refusal(&error, "codex", &packets).is_some());
    assert!(source_exclusive_codex_canonical_refusal(&error, "claude_code", &packets).is_none());
    assert!(source_exclusive_codex_canonical_refusal(&error, "codex", &[]).is_none());
    assert!(source_exclusive_codex_canonical_refusal(
        &error, "codex", &[packet.clone(), packet.clone()],
    ).is_none());
    for field in ["path", "external_id", "provider"] {
        let mut changed = packet.clone();
        match field {
            "path" => changed.source_path = temp.path().join("different.jsonl"),
            "external_id" => changed.external_id = Some("different-source".to_owned()),
            "provider" => changed.agent_slug = "claude_code".to_owned(),
            _ => unreachable!(),
        }
        assert!(source_exclusive_codex_canonical_refusal(&error, "codex", &[changed]).is_none());
    }
    let invalid_data = anyhow::Error::new(std::io::Error::new(
        std::io::ErrorKind::InvalidData, error.to_string(),
    ));
    assert!(source_exclusive_codex_canonical_refusal(&invalid_data, "codex", &packets).is_none());
    let database_error = anyhow::Error::new(crate::franken_sync::FrankenError::Busy);
    assert!(source_exclusive_codex_canonical_refusal(&database_error, "codex", &packets).is_none());
    let close_error = persist::error_after_writer_close_failure(
        refusal(), anyhow::Error::new(crate::franken_sync::FrankenError::Busy),
    );
    assert!(close_error.downcast_ref::<CodexCanonicalRefusal>().is_none());
    assert!(close_error.downcast_ref::<crate::franken_sync::FrankenError>().is_some());
    let historical_error = persist::error_after_writer_close_failure(
        anyhow::anyhow!("ordinary write failure"), anyhow::anyhow!("ordinary close failure"),
    );
    assert_eq!(historical_error.to_string(), "ordinary write failure");
    Ok(())
}
