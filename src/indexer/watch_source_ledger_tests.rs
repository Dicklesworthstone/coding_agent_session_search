//! Exercise provider boundary behavior without opening canonical or lexical
//! storage. The fixture reproduces Codex/Claude's completion rule: a delivered
//! prefix has no completion event if its source changed before certification.

use super::*;
use crate::connectors::{DetectionResult, DiscoveredSourceRole};
use franken_agent_detection::{SourceCompletion, SourceScanHooks};
use serial_test::serial;
use std::io::Write;
use tempfile::TempDir;

#[cfg(unix)]
fn codex_watch_prefix_fixture(temp: &TempDir) -> Result<(PathBuf, ScanContext)> {
    let path = temp.path().join("rollout-watch-prefix.jsonl");
    fs::write(&path, b"{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":\"first\"}}\n")?;
    let ctx = ScanContext::with_roots(
        temp.path().to_path_buf(),
        vec![ScanRoot::local(path.clone())],
        None,
    );
    Ok((path, ctx))
}

#[cfg(unix)]
fn claude_watch_prefix_line(role: &str, content: &str) -> String {
    format!(
        "{}\n",
        serde_json::json!({
            "type": role,
            "timestamp": "2026-11-24T12:00:00Z",
            "sessionId": "watch-prefix-session",
            "cwd": "/workspace/project",
            "message": {"role": role, "content": content}
        })
    )
}

#[cfg(unix)]
fn claude_watch_prefix_fixture(temp: &TempDir) -> Result<(PathBuf, ScanContext)> {
    let root = temp.path().join("projects");
    let project = root.join("watch-project");
    fs::create_dir_all(&project)?;
    let path = project.join("session.jsonl");
    fs::write(&path, claude_watch_prefix_line("user", "first"))?;
    let ctx = ScanContext::with_roots(
        temp.path().join("archive"),
        vec![ScanRoot::local(root)],
        None,
    );
    Ok((path, ctx))
}

#[cfg(unix)]
#[test]
#[serial]
fn claude_prefix_watch_receipt_survives_publication_while_writer_and_debt_remain() -> Result<()> {
    let temp = TempDir::new()?;
    let (path, ctx) = claude_watch_prefix_fixture(&temp)?;
    let mut writer = OpenOptions::new().append(true).open(&path)?;
    let tail = claude_watch_prefix_line("assistant", "unfinished second turn");
    writer.write_all(tail.trim_end_matches('\n').as_bytes())?;
    writer.flush()?;
    let filter = ActiveSessionSourceFilter::with_recent_write_window_for_test(None);
    let mut ledger = WatchSourceLedger::default();
    let mut conversations = Vec::new();
    ledger.scan(
        &crate::connectors::claude_code::ClaudeCodeConnector::new(),
        "claude",
        &ctx,
        &HashMap::new(),
        true,
        false,
        &filter,
        None,
        &mut |conversation| {
            conversations.push(conversation);
            Ok(true)
        },
    )?;
    assert_eq!(conversations.len(), 1);
    assert_eq!(conversations[0].agent_slug, "claude_code");
    assert_eq!(conversations[0].source_path, path);
    assert_eq!(conversations[0].messages.len(), 1);
    assert_eq!(conversations[0].messages[0].content, "first");
    ledger.align_prefix_metadata(&mut conversations[0])?;
    let receipt = conversations[0].metadata["cass"]["claude_stable_prefix"].clone();
    assert_eq!(receipt["provider"], "claude_code");
    assert_eq!(receipt["partial"], true);
    assert_eq!(receipt["message_count"], 1);
    assert_eq!(
        receipt["committed_bytes"],
        claude_watch_prefix_line("user", "first").len() as u64
    );
    assert!(ledger.unstable_sources);
    assert!(ledger.completed_source_paths().next().is_none());
    assert!(ledger.may_publish_source(&path));
    let key = ledger.completions[&path].entry.key.clone();
    let storage = FrankenStorage::open(&temp.path().join("archive.db"))?;
    let debt = WatchLexicalReplayDebt::for_root(ConnectorKind::Claude, &ctx.scan_roots[0]);
    ledger.begin_publication(&storage, &debt, None)?;
    let admitted = storage.source_ingest_ledger_entries()?;
    let value: serde_json::Value = serde_json::from_str(&admitted[&key])?;
    assert_eq!(value["publication_invalidated"], true);
    assert_eq!(value["watch_published"], false);
    assert_eq!(value["claude_prefix"], receipt);
    assert!(value.get("codex_prefix").is_none());
    assert!(!load_watch_lexical_replay_debt(&storage)?.is_empty());

    // Invoke the real ledger acknowledgement used after canonical+lexical
    // commit. The active tail retains replay debt without losing its proof.
    ledger.publish_batch(&storage, &conversations, &[], true)?;
    let published = storage.source_ingest_ledger_entries()?;
    let value: serde_json::Value = serde_json::from_str(&published[&key])?;
    assert_eq!(value["watch_published"], true);
    assert!(value.get("publication_invalidated").is_none());
    assert_eq!(value["claude_prefix"], receipt);
    assert!(!load_watch_lexical_replay_debt(&storage)?.is_empty());
    assert!(!source_ledger_matches_with_contract(
        &published[&key],
        &DiscoveredSourceFile::new(
            "claude_code",
            &ctx.scan_roots[0],
            path.clone(),
            DiscoveredSourceRole::PrimarySessionLog,
            true,
        )
        .with_fs_metadata(),
        &source_ledger_producer_contract("claude")?.0,
    ));
    assert!(fs::read_to_string(&path)?.ends_with(tail.trim_end_matches('\n')));
    Ok(())
}

#[cfg(unix)]
#[test]
#[serial]
fn claude_prefix_watch_exact_lookup_preserves_omitted_overlap_and_discovers_appends() -> Result<()>
{
    let temp = TempDir::new()?;
    let (path, ctx) = claude_watch_prefix_fixture(&temp)?;
    let mut writer = OpenOptions::new().append(true).open(&path)?;
    let connector = crate::connectors::claude_code::ClaudeCodeConnector::new();
    let filter = ActiveSessionSourceFilter::with_recent_write_window_for_test(None);
    let mut initial = WatchSourceLedger::default();
    let mut first = Vec::new();
    initial.scan_with_ledger_lookup(
        &connector,
        "claude",
        &ctx,
        &HashMap::new(),
        true,
        false,
        &filter,
        None,
        &mut |_| panic!("a complete empty cache needs no database fallback"),
        &mut |conversation| {
            first.push(conversation);
            Ok(true)
        },
    )?;
    assert_eq!(first.len(), 1);
    let key = initial.completions[&path].entry.key.clone();
    let storage = FrankenStorage::open(&temp.path().join("archive.db"))?;
    let debt = WatchLexicalReplayDebt::for_root(ConnectorKind::Claude, &ctx.scan_roots[0]);
    initial.begin_publication(&storage, &debt, None)?;
    initial.publish_batch(&storage, &first, &[], true)?;
    let original = storage.source_ingest_ledger_entries()?;
    let cache = HashMap::from([(
        super::super::watch_source_cache::INCOMPLETE_SOURCE_LEDGER_KEY.to_string(),
        String::new(),
    )]);
    writer.write_all(claude_watch_prefix_line("assistant", "second").as_bytes())?;
    writer.flush()?;
    let mut next = WatchSourceLedger::default();
    let mut lookups = 0;
    let mut delivered = Vec::new();
    next.scan_with_ledger_lookup(
        &connector,
        "claude",
        &ctx,
        &cache,
        true,
        false,
        &filter,
        None,
        &mut |observed_key| {
            lookups += 1;
            assert_eq!(observed_key, key);
            super::super::watch_source_cache::lookup_watch_source_observation(
                &storage,
                observed_key,
                None,
            )
        },
        &mut |conversation| {
            delivered.push(conversation);
            Ok(true)
        },
    )?;
    assert_eq!(lookups, 1);
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].agent_slug, "claude_code");
    assert_eq!(delivered[0].external_id, first[0].external_id);
    assert_eq!(delivered[0].messages.len(), 2);
    assert_eq!(delivered[0].messages[0].content, "first");
    assert_eq!(delivered[0].messages[1].content, "second");
    assert_eq!(
        next.prefix_invalidations[&key],
        invalidated_prefix_observation(&original[&key])?.unwrap()
    );
    assert_eq!(
        cache.len(),
        1,
        "exact lookups must not grow the bounded cache"
    );
    assert_eq!(storage.source_ingest_ledger_entries()?, original);

    fs::write(&path, fs::read_to_string(&path)?.replace("first", "other"))?;
    let mut refused = WatchSourceLedger::default();
    let mut called = false;
    assert!(
        refused
            .scan_with_ledger_lookup(
                &connector,
                "claude",
                &ctx,
                &cache,
                true,
                false,
                &filter,
                None,
                &mut |observed_key| {
                    super::super::watch_source_cache::lookup_watch_source_observation(
                        &storage,
                        observed_key,
                        None,
                    )
                },
                &mut |_| {
                    called = true;
                    Ok(true)
                },
            )
            .is_err()
    );
    assert!(
        !called,
        "an omitted Claude proof must still reject rewritten admitted bytes"
    );
    assert!(refused.admitted_keys.is_empty());
    assert_eq!(storage.source_ingest_ledger_entries()?, original);
    Ok(())
}

#[cfg(unix)]
#[test]
#[serial]
fn claude_prefix_watch_alignment_requires_original_evidence_and_allows_only_downgrade() -> Result<()>
{
    let temp = TempDir::new()?;
    let (path, ctx) = claude_watch_prefix_fixture(&temp)?;
    File::open(&path)?.set_modified(SystemTime::now() - Duration::from_secs(7200))?;
    let filter = ActiveSessionSourceFilter::with_recent_write_window_for_test(None);
    let mut ledger = WatchSourceLedger::default();
    let mut conversations = Vec::new();
    ledger.scan(
        &crate::connectors::claude_code::ClaudeCodeConnector::new(),
        "claude",
        &ctx,
        &HashMap::new(),
        true,
        false,
        &filter,
        None,
        &mut |conversation| {
            conversations.push(conversation);
            Ok(true)
        },
    )?;
    assert_eq!(conversations.len(), 1);
    assert_eq!(ledger.completed_source_paths().count(), 1);
    let conversation = &mut conversations[0];
    let original = conversation.metadata.clone();
    assert_eq!(original["cass"]["claude_stable_prefix"]["partial"], false);
    ledger.align_prefix_metadata(conversation)?;
    assert_eq!(conversation.metadata, original);
    for mismatch in [
        "missing_cass",
        "missing_marker",
        "provider",
        "path",
        "count",
        "bytes",
        "digest",
        "promote",
    ] {
        conversation.metadata = original.clone();
        match mismatch {
            "missing_cass" => conversation.metadata["cass"] = serde_json::Value::Null,
            "missing_marker" => {
                conversation.metadata["cass"]
                    .as_object_mut()
                    .unwrap()
                    .remove("claude_stable_prefix");
            }
            "provider" => {
                conversation.metadata["cass"]["claude_stable_prefix"]["provider"] = "codex".into()
            }
            "path" => {
                conversation.metadata["cass"]["claude_stable_prefix"]["source_path"] =
                    serde_json::json!(temp.path().join("other.jsonl"))
            }
            "count" => {
                conversation.metadata["cass"]["claude_stable_prefix"]["message_count"] = 2.into()
            }
            "bytes" => {
                conversation.metadata["cass"]["claude_stable_prefix"]["committed_bytes"] = 0.into()
            }
            "digest" => {
                conversation.metadata["cass"]["claude_stable_prefix"]["messages_blake3"] =
                    "0".repeat(64).into()
            }
            "promote" => {
                conversation.metadata["cass"]["claude_stable_prefix"]["partial"] = true.into()
            }
            _ => unreachable!(),
        }
        assert!(
            ledger.align_prefix_metadata(conversation).is_err(),
            "accepted {mismatch}"
        );
    }

    conversation.metadata = original.clone();
    conversation.source_path = temp.path().join("different-source.jsonl");
    assert!(ledger.align_prefix_metadata(conversation).is_err());
    conversation.source_path = path.clone();
    let mut completion = ledger.completions.remove(&path).unwrap();
    assert!(
        ledger.align_prefix_metadata(conversation).is_err(),
        "a typed packet cannot publish without its source completion"
    );
    completion.certified = false;
    ledger.completions.insert(path.clone(), completion);
    assert!(
        ledger.align_prefix_metadata(conversation).is_err(),
        "provisional delivery is not certified publication evidence"
    );
    let completion = ledger.completions.get_mut(&path).unwrap();
    completion.certified = true;
    let original_observation = completion.entry.observation.clone();
    let mut without_receipt: serde_json::Value = serde_json::from_str(&original_observation)?;
    without_receipt
        .as_object_mut()
        .unwrap()
        .remove("claude_prefix");
    completion.entry.observation = serde_json::to_string(&without_receipt)?;
    assert!(
        ledger.align_prefix_metadata(conversation).is_err(),
        "a completion without its typed proof cannot replace parser evidence"
    );
    ledger.completions.get_mut(&path).unwrap().entry.observation = original_observation;

    conversation.metadata = original.clone();
    let mut writer = OpenOptions::new().append(true).open(&path)?;
    writer.write_all(claude_watch_prefix_line("assistant", "second").as_bytes())?;
    writer.flush()?;
    ledger.revalidate_prefixes(&filter, None)?;
    assert!(ledger.may_publish_source(&path));
    assert!(ledger.completed_source_paths().next().is_none());
    ledger.align_prefix_metadata(conversation)?;
    let mut downgraded = original;
    downgraded["cass"]["claude_stable_prefix"]["partial"] = true.into();
    assert_eq!(conversation.metadata, downgraded);
    assert_eq!(conversation.messages.len(), 1);
    assert_eq!(conversation.messages[0].content, "first");
    Ok(())
}

#[cfg(unix)]
#[test]
#[serial]
fn codex_prefix_watch_receipt_is_durable_without_whole_source_coverage() -> Result<()> {
    let temp = TempDir::new()?;
    let (path, ctx) = codex_watch_prefix_fixture(&temp)?;
    let _writer = OpenOptions::new().append(true).open(&path)?;
    let filter = ActiveSessionSourceFilter::with_recent_write_window_for_test(None);
    let mut ledger = WatchSourceLedger::default();
    let mut conversations = Vec::new();
    ledger.scan(
        &crate::connectors::codex::CodexConnector::new(),
        "codex",
        &ctx,
        &HashMap::new(),
        true,
        false,
        &filter,
        None,
        &mut |conversation| {
            conversations.push(conversation);
            Ok(true)
        },
    )?;
    assert_eq!(conversations.len(), 1);
    assert!(ledger.unstable_sources);
    assert!(ledger.completed_source_paths().next().is_none());
    assert!(ledger.may_publish_source(&path));
    let key = ledger.completions[&path].entry.key.clone();
    let storage = FrankenStorage::open(&temp.path().join("archive.db"))?;
    let debt = WatchLexicalReplayDebt::for_root(ConnectorKind::Codex, &ctx.scan_roots[0]);
    ledger.begin_publication(&storage, &debt, None)?;
    let admitted = storage.source_ingest_ledger_entries()?;
    let value: serde_json::Value = serde_json::from_str(&admitted[&key])?;
    assert_eq!(value["publication_invalidated"], true);
    assert_eq!(value["watch_published"], false);
    assert_eq!(value["codex_prefix"]["partial"], true);
    assert!(!load_watch_lexical_replay_debt(&storage)?.is_empty());
    // This is the ledger operation invoked after canonical+lexical commit.
    // Keeping root-wide recovery debt must not discard the partial receipt.
    ledger.publish_batch(&storage, &conversations, &[], true)?;
    let published = storage.source_ingest_ledger_entries()?;
    let value: serde_json::Value = serde_json::from_str(&published[&key])?;
    assert_eq!(value["watch_published"], true);
    assert!(value.get("publication_invalidated").is_none());
    assert_eq!(value["codex_prefix"]["partial"], true);
    assert!(!load_watch_lexical_replay_debt(&storage)?.is_empty());
    assert!(!source_ledger_matches_with_contract(
        &published[&key],
        &DiscoveredSourceFile::new(
            "codex",
            &ctx.scan_roots[0],
            path.clone(),
            DiscoveredSourceRole::PrimarySessionLog,
            true,
        )
        .with_fs_metadata(),
        &source_ledger_producer_contract("codex")?.0,
    ));
    fs::write(&path, fs::read_to_string(&path)?.replace("first", "other"))?;
    let mut retry = WatchSourceLedger::default();
    let mut called = false;
    assert!(
        retry
            .scan(
                &crate::connectors::codex::CodexConnector::new(),
                "codex",
                &ctx,
                &published,
                true,
                true,
                &filter,
                None,
                &mut |_| {
                    called = true;
                    Ok(true)
                },
            )
            .is_err()
    );
    assert!(
        !called,
        "persisted overlap must reject a rewritten first message"
    );
    assert_eq!(storage.source_ingest_ledger_entries()?, published);
    Ok(())
}

#[cfg(unix)]
#[test]
#[serial]
fn codex_prefix_watch_drops_spooled_delivery_when_final_validation_fails() -> Result<()> {
    let temp = TempDir::new()?;
    let (path, ctx) = codex_watch_prefix_fixture(&temp)?;
    let filter = ActiveSessionSourceFilter::with_recent_write_window_for_test(None);
    let mut ledger = WatchSourceLedger::default();
    let mut spool = WatchConversationSpool::new(temp.path(), 1);
    let result = ledger.scan(
        &crate::connectors::codex::CodexConnector::new(),
        "codex",
        &ctx,
        &HashMap::new(),
        true,
        false,
        &filter,
        None,
        &mut |conversation| {
            spool.push(conversation, None)?;
            fs::write(&path, fs::read_to_string(&path)?.replace("first", "other"))?;
            Ok(true)
        },
    );
    assert!(result.is_err());
    assert_eq!(spool.len(), 1, "delivery was provisional");
    ledger.revalidate_prefixes(&filter, None)?;
    spool
        .entries
        .retain(|entry| ledger.may_publish_source(&entry.key.source_path));
    assert_eq!(spool.len(), 0);
    assert!(ledger.unstable_sources);
    assert!(ledger.completed_source_paths().next().is_none());
    assert!(filter.take_deferred_sources().contains(&path));
    Ok(())
}

#[cfg(unix)]
#[test]
#[serial]
fn codex_prefix_watch_cannot_deliver_when_receipt_capacity_is_exhausted() -> Result<()> {
    let temp = TempDir::new()?;
    let (path, ctx) = codex_watch_prefix_fixture(&temp)?;
    let filter = ActiveSessionSourceFilter::with_recent_write_window_for_test(None);
    let mut ledger = WatchSourceLedger {
        completion_bytes: MAX_RECEIPT_BYTES,
        ..Default::default()
    };
    let mut called = false;
    ledger.scan(
        &crate::connectors::codex::CodexConnector::new(),
        "codex",
        &ctx,
        &HashMap::new(),
        true,
        false,
        &filter,
        None,
        &mut |_| {
            called = true;
            Ok(true)
        },
    )?;
    assert!(!called);
    assert!(ledger.completions.is_empty());
    assert!(ledger.unstable_sources);
    assert!(filter.take_deferred_sources().contains(&path));
    Ok(())
}

#[cfg(unix)]
#[test]
#[serial]
fn codex_prefix_watch_missing_integrity_cache_entry_defers_without_invalidation() -> Result<()> {
    let temp = TempDir::new()?;
    let (path, ctx) = codex_watch_prefix_fixture(&temp)?;
    let filter = ActiveSessionSourceFilter::with_recent_write_window_for_test(None);
    let mut ledger = WatchSourceLedger::default();
    let saved = HashMap::from([(
        super::super::watch_source_cache::INCOMPLETE_SOURCE_LEDGER_KEY.to_string(),
        String::new(),
    )]);
    let mut called = false;
    ledger.scan(
        &crate::connectors::codex::CodexConnector::new(),
        "codex",
        &ctx,
        &saved,
        true,
        false,
        &filter,
        None,
        &mut |_| {
            called = true;
            Ok(true)
        },
    )?;
    assert!(!called);
    assert!(ledger.admitted_keys.is_empty());
    assert!(ledger.unstable_sources);
    assert!(filter.take_deferred_sources().contains(&path));
    Ok(())
}

#[cfg(unix)]
#[test]
#[serial]
fn codex_prefix_watch_exact_lookup_admits_new_source_despite_unrelated_cache_omission() -> Result<()> {
    let temp = TempDir::new()?;
    let (path, ctx) = codex_watch_prefix_fixture(&temp)?;
    let storage = FrankenStorage::open(&temp.path().join("archive.db"))?;
    let unrelated = "x".repeat(super::super::watch_source_cache::MAX_WATCH_SOURCE_LEDGER_VALUE_BYTES + 1);
    storage.raw().execute_compat(
        "INSERT INTO meta(key, value) VALUES('source_ingest_v1:unrelated', ?1)",
        &[ParamValue::from(unrelated.as_str())],
    )?;
    let cache = super::super::watch_source_cache::load_bounded_watch_source_ledger(&storage, None)?;
    assert!(cache.contains_key(super::super::watch_source_cache::INCOMPLETE_SOURCE_LEDGER_KEY));
    let filter = ActiveSessionSourceFilter::with_recent_write_window_for_test(None);
    let mut ledger = WatchSourceLedger::default();
    let mut lookups = 0;
    let mut delivered = Vec::new();
    ledger.scan_with_ledger_lookup(
        &crate::connectors::codex::CodexConnector::new(), "codex", &ctx, &cache,
        true, false, &filter, None,
        &mut |key| {
            lookups += 1;
            super::super::watch_source_cache::lookup_watch_source_observation(&storage, key, None)
        },
        &mut |conversation| { delivered.push(conversation); Ok(true) },
    )?;
    assert_eq!(lookups, 1);
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].source_path, path);
    assert_eq!(delivered[0].messages[0].content, "first");
    assert!(ledger.completions[&path].certified);
    assert_eq!(cache.len(), 1, "point lookups must not grow the cache");
    assert_eq!(storage.source_ingest_ledger_entries()?.len(), 1, "lookup is read-only");
    Ok(())
}

#[cfg(unix)]
#[test]
#[serial]
fn codex_prefix_watch_exact_lookup_preserves_omitted_overlap_and_discovers_appends() -> Result<()> {
    let temp = TempDir::new()?;
    let (path, ctx) = codex_watch_prefix_fixture(&temp)?;
    let mut writer = OpenOptions::new().append(true).open(&path)?;
    let connector = crate::connectors::codex::CodexConnector::new();
    let filter = ActiveSessionSourceFilter::with_recent_write_window_for_test(None);
    let mut initial = WatchSourceLedger::default();
    initial.scan_with_ledger_lookup(
        &connector, "codex", &ctx, &HashMap::new(), true, false, &filter, None,
        &mut |_| panic!("a complete empty cache needs no database fallback"),
        &mut |_| Ok(true),
    )?;
    let entry = &initial.completions[&path].entry;
    let storage = FrankenStorage::open(&temp.path().join("archive.db"))?;
    storage.raw().execute_compat(
        "INSERT INTO meta(key, value) VALUES(?1, ?2)",
        &[ParamValue::from(entry.key.as_str()), ParamValue::from(entry.observation.as_str())],
    )?;
    let original = storage.source_ingest_ledger_entries()?;
    let cache = HashMap::from([(
        super::super::watch_source_cache::INCOMPLETE_SOURCE_LEDGER_KEY.to_string(), String::new(),
    )]);
    writer.write_all(b"{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":\"second\"}}\n")?;
    let mut next = WatchSourceLedger::default();
    let mut delivered = Vec::new();
    next.scan_with_ledger_lookup(
        &connector, "codex", &ctx, &cache, true, false, &filter, None,
        &mut |key| super::super::watch_source_cache::lookup_watch_source_observation(&storage, key, None),
        &mut |conversation| { delivered.push(conversation); Ok(true) },
    )?;
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].messages.len(), 2);
    assert_eq!(next.prefix_invalidations[&entry.key], invalidated_prefix_observation(&entry.observation)?.unwrap());
    assert_eq!(storage.source_ingest_ledger_entries()?, original);
    fs::write(&path, fs::read_to_string(&path)?.replace("first", "other"))?;
    let mut refused = WatchSourceLedger::default();
    let mut called = false;
    assert!(refused.scan_with_ledger_lookup(
        &connector, "codex", &ctx, &cache, true, false, &filter, None,
        &mut |key| super::super::watch_source_cache::lookup_watch_source_observation(&storage, key, None),
        &mut |_| { called = true; Ok(true) },
    ).is_err());
    assert!(!called, "an omitted proof must still reject changed admitted bytes");
    assert!(refused.admitted_keys.is_empty());
    assert_eq!(storage.source_ingest_ledger_entries()?, original);
    Ok(())
}

#[cfg(unix)]
#[test]
#[serial]
fn codex_prefix_watch_exact_lookup_refuses_only_the_oversized_source_receipt() -> Result<()> {
    let temp = TempDir::new()?;
    let (refused, _) = codex_watch_prefix_fixture(&temp)?;
    let healthy = temp.path().join("rollout-z-healthy.jsonl");
    fs::write(&healthy, fs::read(&refused)?)?;
    let ctx = ScanContext::with_roots(
        temp.path().to_path_buf(), vec![ScanRoot::local(temp.path().to_path_buf())], None,
    );
    let connector = crate::connectors::codex::CodexConnector::new();
    let source = connector.discover_source_files(&ctx)?.into_iter()
        .find(|source| source.source_path == refused).context("refused fixture source")?;
    let key = source_ledger_key(&source, &ctx).context("refused fixture key")?;
    let storage = FrankenStorage::open(&temp.path().join("archive.db"))?;
    storage.raw().execute_compat(
        "INSERT INTO meta(key, value) VALUES(?1, ?2)",
        &[ParamValue::from(key.as_str()), ParamValue::from("x".repeat(
            super::super::watch_source_cache::MAX_WATCH_SOURCE_LEDGER_VALUE_BYTES + 1,
        ))],
    )?;
    let cache = super::super::watch_source_cache::load_bounded_watch_source_ledger(&storage, None)?;
    let filter = ActiveSessionSourceFilter::with_recent_write_window_for_test(None);
    let mut ledger = WatchSourceLedger::default();
    let mut delivered = Vec::new();
    ledger.scan_with_ledger_lookup(
        &connector, "codex", &ctx, &cache, true, false, &filter, None,
        &mut |key| super::super::watch_source_cache::lookup_watch_source_observation(&storage, key, None),
        &mut |conversation| { delivered.push(conversation.source_path); Ok(true) },
    )?;
    assert_eq!(delivered, vec![healthy]);
    assert!(ledger.unstable_sources);
    assert!(!ledger.admitted_keys.contains(&key));
    assert!(filter.take_deferred_sources().contains(&refused));
    assert_eq!(storage.source_ingest_ledger_entries()?.len(), 1);
    Ok(())
}

#[cfg(unix)]
#[test]
#[serial]
fn codex_prefix_watch_exact_lookup_database_failure_preserves_unadmitted_source() -> Result<()> {
    let temp = TempDir::new()?;
    let (path, ctx) = codex_watch_prefix_fixture(&temp)?;
    let cache = HashMap::from([(
        super::super::watch_source_cache::INCOMPLETE_SOURCE_LEDGER_KEY.to_string(), String::new(),
    )]);
    let filter = ActiveSessionSourceFilter::with_recent_write_window_for_test(None);
    let mut ledger = WatchSourceLedger::default();
    let mut called = false;
    let error = ledger.scan_with_ledger_lookup(
        &crate::connectors::codex::CodexConnector::new(), "codex", &ctx, &cache,
        true, false, &filter, None,
        &mut |_| anyhow::bail!("fixture database lookup failure"),
        &mut |_| { called = true; Ok(true) },
    ).err().context("database failure was treated as absent evidence")?;
    assert!(format!("{error:#}").contains("fixture database lookup failure"));
    assert!(!called);
    assert!(ledger.admitted_keys.is_empty());
    assert!(ledger.unstable_sources);
    assert!(filter.take_deferred_sources().contains(&path));
    Ok(())
}

#[cfg(unix)]
#[test]
#[serial]
fn codex_prefix_watch_refused_proof_does_not_starve_later_siblings() -> Result<()> {
    for malformed in [false, true] {
        for omitted in [false, true] {
            let temp = TempDir::new()?;
            let (refused, _) = codex_watch_prefix_fixture(&temp)?;
            let ctx = ScanContext::with_roots(temp.path().to_path_buf(),
                vec![ScanRoot::local(temp.path().to_path_buf())], None);
            let connector = crate::connectors::codex::CodexConnector::new();
            let filter = ActiveSessionSourceFilter::with_recent_write_window_for_test(None);
            let mut initial = WatchSourceLedger::default();
            initial.scan(&connector, "codex", &ctx, &HashMap::new(), true, false,
                &filter, None, &mut |_| Ok(true))?;
            let entry = &initial.completions[&refused].entry;
            let key = entry.key.clone();
            let observation = if malformed {
                serde_json::json!({"codex_prefix": "malformed"}).to_string()
            } else {
                fs::write(&refused, fs::read_to_string(&refused)?.replace("first", "other"))?;
                entry.observation.clone()
            };
            let healthy = temp.path().join("rollout-z-healthy.jsonl");
            fs::write(&healthy, fs::read(&refused)?)?;
            assert_eq!(connector.discover_source_files(&ctx)?[0].source_path, refused);
            let storage = FrankenStorage::open(&temp.path().join("archive.db"))?;
            storage.raw().execute_compat("INSERT INTO meta(key, value) VALUES(?1, ?2)", &[
                ParamValue::from(key.as_str()), ParamValue::from(observation.as_str()),
            ])?;
            let saved = if omitted {
                HashMap::from([(
                    super::super::watch_source_cache::INCOMPLETE_SOURCE_LEDGER_KEY.to_string(),
                    String::new(),
                )])
            } else {
                super::super::watch_source_cache::load_bounded_watch_source_ledger(&storage, None)?
            };
            filter.take_deferred_sources();
            let mut ledger = WatchSourceLedger::default();
            let mut delivered = Vec::new();
            let error = ledger.scan_with_ledger_lookup(&connector, "codex", &ctx, &saved,
                true, false, &filter, None,
                &mut |key| super::super::watch_source_cache::lookup_watch_source_observation(&storage, key, None),
                &mut |conversation| { delivered.push(conversation); Ok(true) },
            ).err().context("refused proof certified complete coverage")?;
            assert_eq!(delivered.len(), 1, "malformed={malformed}, omitted={omitted}");
            assert_eq!(delivered[0].source_path, healthy);
            assert!(ledger.completions[&healthy].certified);
            assert!(!ledger.admitted_keys.contains(&key));
            assert!(ledger.unstable_sources);
            assert!(filter.take_deferred_sources().contains(&refused));
            let mut failures = Vec::new();
            assert!(crate::connectors::codex::for_each_source_failure(&error, |path, reason| {
                failures.push((path.to_path_buf(), reason));
            }));
            assert_eq!(failures, vec![(refused.clone(), if malformed {
                crate::connectors::codex::SourceFailureReason::InvalidSourceData
            } else {
                crate::connectors::codex::SourceFailureReason::SourceChanged
            })]);
            // Real watch publication can proceed for the certified sibling,
            // but must neither invalidate nor replace the rejected proof.
            let debt = WatchLexicalReplayDebt::for_root(ConnectorKind::Codex, &ctx.scan_roots[0]);
            ledger.begin_publication(&storage, &debt, None)?;
            assert_eq!(storage.source_ingest_ledger_entries()?[&key], observation);
            assert!(!load_watch_lexical_replay_debt(&storage)?.is_empty());
        }
    }
    Ok(())
}

#[cfg(unix)]
#[test]
#[serial]
fn codex_prefix_watch_terminal_failures_still_stop_after_source_refusal() -> Result<()> {
    for database_failure in [false, true] {
        let temp = TempDir::new()?;
        let (refused, _) = codex_watch_prefix_fixture(&temp)?;
        let first = temp.path().join("rollout-y-terminal-failure.jsonl");
        let later = temp.path().join("rollout-z-must-not-deliver.jsonl");
        for path in [&first, &later] {
            fs::write(path, fs::read(&refused)?)?;
        }
        let ctx = ScanContext::with_roots(temp.path().to_path_buf(),
            vec![ScanRoot::local(temp.path().to_path_buf())], None);
        let connector = crate::connectors::codex::CodexConnector::new();
        let source = connector.discover_source_files(&ctx)?.into_iter()
            .find(|source| source.source_path == refused).context("refused source")?;
        let key = source_ledger_key(&source, &ctx).context("refused source key")?;
        let saved = HashMap::from([
            (key.clone(), serde_json::json!({"codex_prefix": "malformed"}).to_string()),
            (super::super::watch_source_cache::INCOMPLETE_SOURCE_LEDGER_KEY.to_string(), String::new()),
        ]);
        let filter = ActiveSessionSourceFilter::with_recent_write_window_for_test(None);
        let mut ledger = WatchSourceLedger::default();
        let mut lookups = 0;
        let mut delivered = Vec::new();
        let error = ledger.scan_with_ledger_lookup(&connector, "codex", &ctx, &saved,
            true, false, &filter, None,
            &mut |_| {
                lookups += 1;
                anyhow::ensure!(!database_failure, "terminal metadata database failure");
                Ok(SourceObservationLookup::Absent)
            },
            &mut |conversation| {
                delivered.push(conversation.source_path);
                Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "terminal spool failure").into())
            },
        ).err().context("terminal failure was hidden")?;
        assert_eq!(lookups, 1, "a terminal error must stop later source admission");
        if database_failure {
            assert!(delivered.is_empty());
            assert!(ledger.admitted_keys.is_empty());
            assert!(format!("{error:#}").contains("terminal metadata database failure"));
        } else {
            assert_eq!(delivered, vec![first]);
            assert_eq!(error.downcast_ref::<std::io::Error>().unwrap().kind(), std::io::ErrorKind::BrokenPipe);
        }
        assert!(!ledger.admitted_keys.contains(&key));
        assert!(ledger.unstable_sources);
        assert!(filter.take_deferred_sources().contains(&refused));
        let mut refused_paths = Vec::new();
        assert!(crate::connectors::codex::for_each_source_failure(&error, |path, _| {
            refused_paths.push(path.to_path_buf());
        }));
        assert_eq!(refused_paths, vec![refused]);
    }
    Ok(())
}
#[cfg(unix)]
#[test]
#[serial]
fn codex_prefix_watch_late_append_downgrades_coverage_before_publication() -> Result<()> {
    let temp = TempDir::new()?;
    let (path, ctx) = codex_watch_prefix_fixture(&temp)?;
    File::open(&path)?.set_modified(SystemTime::now() - Duration::from_secs(7200))?;
    let filter = ActiveSessionSourceFilter::with_recent_write_window_for_test(None);
    let mut ledger = WatchSourceLedger::default();
    ledger.scan(
        &crate::connectors::codex::CodexConnector::new(),
        "codex",
        &ctx,
        &HashMap::new(),
        true,
        false,
        &filter,
        None,
        &mut |_| Ok(true),
    )?;
    assert_eq!(ledger.completed_source_paths().count(), 1);
    let mut writer = OpenOptions::new().append(true).open(&path)?;
    writer.write_all(b"{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":\"second\"}}\n")?;
    ledger.revalidate_prefixes(&filter, None)?;
    assert!(ledger.may_publish_source(&path));
    assert!(ledger.completed_source_paths().next().is_none());
    assert!(ledger.unstable_sources);
    let value: serde_json::Value =
        serde_json::from_str(&ledger.completions[&path].entry.observation)?;
    assert_eq!(value["codex_prefix"]["partial"], true);
    assert_eq!(value["codex_prefix"]["message_count"], 1);
    Ok(())
}

struct WithheldCompletionConnector {
    provider: &'static str,
    root: ScanRoot,
    paths: Vec<PathBuf>,
    changing: PathBuf,
    append_once: Cell<bool>,
    withheld: Cell<usize>,
}

impl Connector for WithheldCompletionConnector {
    fn detect(&self) -> DetectionResult {
        DetectionResult {
            detected: true,
            evidence: Vec::new(),
            root_paths: vec![self.root.path.clone()],
        }
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let mut conversations = Vec::new();
        self.scan_with_source_boundaries(
            ctx,
            &mut SourceScanHooks::default(),
            &mut |conversation| {
                conversations.push(conversation);
                Ok(())
            },
        )?;
        Ok(conversations)
    }

    fn supports_source_boundaries(&self) -> bool {
        true
    }

    fn discover_source_files(&self, _ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        Ok(self
            .paths
            .iter()
            .map(|path| {
                DiscoveredSourceFile::new(
                    self.provider,
                    &self.root,
                    path.clone(),
                    DiscoveredSourceRole::PrimarySessionLog,
                    true,
                )
                .with_fs_metadata()
            })
            .collect())
    }

    fn scan_with_source_boundaries(
        &self,
        ctx: &ScanContext,
        hooks: &mut SourceScanHooks<'_>,
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        assert_eq!(ctx.since_ts, None);
        for source in self.discover_source_files(ctx)? {
            if !hooks.should_scan(&source) {
                continue;
            }
            let content = fs::read_to_string(&source.source_path)?;
            on_conversation(NormalizedConversation {
                agent_slug: self.provider.to_string(),
                external_id: None,
                title: None,
                workspace: None,
                source_path: source.source_path.clone(),
                started_at: None,
                ended_at: None,
                metadata: serde_json::json!({}),
                messages: vec![NormalizedMessage {
                    idx: 0,
                    role: "user".to_string(),
                    author: None,
                    created_at: None,
                    content,
                    extra: serde_json::json!({}),
                    snippets: Vec::new(),
                    invocations: Vec::new(),
                }],
            })?;
            if source.source_path == self.changing && self.append_once.replace(false) {
                // A size change is observable on every supported filesystem;
                // this needs neither a sleep nor a timestamp-granularity bet.
                OpenOptions::new()
                    .append(true)
                    .open(&source.source_path)?
                    .write_all(b" plus newly appended tail")?;
            }
            if source.fs_metadata_changed() {
                self.withheld.set(self.withheld.get() + 1);
            } else {
                hooks.complete(&SourceCompletion {
                    source,
                    required_sidecars: Vec::new(),
                    conversations_emitted: 1,
                })?;
            }
        }
        Ok(())
    }
}

fn detect_withheld_completion(next_source: bool) -> Result<()> {
    for (connector_name, provider) in [("codex", "codex"), ("claude", "claude_code")] {
        let temp = TempDir::new()?;
        let changing = temp.path().join("rollout-changing.jsonl");
        fs::write(&changing, b"delivered prefix")?;
        let stable = temp.path().join("rollout-stable.jsonl");
        let mut paths = vec![changing.clone()];
        if next_source {
            fs::write(&stable, b"stable sibling")?;
            paths.push(stable.clone());
        }
        let root = ScanRoot::local(temp.path().to_path_buf());
        let ctx = ScanContext::with_roots(temp.path().to_path_buf(), vec![root.clone()], Some(1));
        let connector = WithheldCompletionConnector {
            provider,
            root,
            paths,
            changing: changing.clone(),
            append_once: Cell::new(true),
            withheld: Cell::new(0),
        };
        let filter = ActiveSessionSourceFilter::with_recent_write_window_for_test(None);
        let mut ledger = WatchSourceLedger::default();
        let mut delivered = Vec::new();
        ledger.scan(
            &connector,
            connector_name,
            &ctx,
            &HashMap::new(),
            true,
            false,
            &filter,
            None,
            &mut |conversation| {
                assert_eq!(conversation.messages.len(), 1);
                delivered.push((
                    conversation.source_path,
                    conversation.messages[0].content.clone(),
                ));
                Ok(true)
            },
        )?;
        let mut expected = vec![(changing.clone(), "delivered prefix".to_string())];
        if next_source {
            expected.push((stable.clone(), "stable sibling".to_string()));
        }
        assert_eq!(delivered, expected, "each source prefix is delivered once");
        assert_eq!(connector.withheld.get(), 1);
        assert!(
            ledger.unstable_sources,
            "a provider's omitted completion must keep the changing source eligible"
        );
        assert!(!ledger.completions.contains_key(&changing));
        assert_eq!(ledger.completions.len(), usize::from(next_source));
        assert!(filter.take_deferred_sources().is_empty());
        if next_source {
            assert!(ledger.completions.contains_key(&stable));
        }

        // The following scan observes the complete bytes and emits completion.
        // Instability must not become sticky after the source has settled.
        let mut retry = WatchSourceLedger::default();
        let mut delivered = Vec::new();
        retry.scan(
            &connector,
            connector_name,
            &ctx,
            &HashMap::new(),
            true,
            false,
            &filter,
            None,
            &mut |conversation| {
                delivered.push((
                    conversation.source_path,
                    conversation.messages[0].content.clone(),
                ));
                Ok(true)
            },
        )?;
        expected[0].1 = "delivered prefix plus newly appended tail".to_string();
        assert_eq!(delivered, expected);
        assert!(!retry.unstable_sources);
        assert_eq!(connector.withheld.get(), 1);
        assert_eq!(retry.completions.len(), expected.len());
        assert_eq!(retry.completions[&changing].remaining, 1);
        assert!(filter.take_deferred_sources().is_empty());
    }
    Ok(())
}

#[test]
#[serial]
fn gh538_watch_ledger_detects_withheld_completion_at_scan_end() -> Result<()> {
    detect_withheld_completion(false)
}

#[test]
#[serial]
fn gh538_watch_ledger_detects_withheld_completion_at_next_source() -> Result<()> {
    detect_withheld_completion(true)
}

const PI_FAMILY_CONTRACT_CHILD: &str = "CASS_TEST_WATCH_PI_FAMILY_CONTRACT_CHILD";
const PI_FAMILY_CONTRACT_STATE: &str = "CASS_TEST_WATCH_PI_FAMILY_CONTRACT_STATE";

fn write_pi_family_contract_session(state: &Path) -> Result<PathBuf> {
    let sessions = state.join("launch-sessions");
    fs::create_dir_all(&sessions)?;
    let path = sessions.join("2026-08-24T12-00-00_watch.jsonl");
    fs::write(
        &path,
        concat!(
            "{\"type\":\"session\",\"version\":3,\"id\":\"watch-contract\",\"timestamp\":\"2026-08-24T12:00:00Z\",\"cwd\":\"/work\"}\n",
            "{\"type\":\"message\",\"timestamp\":\"2026-08-24T12:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"keep the latest profile\"}}\n",
        ),
    )?;
    Ok(path)
}

fn run_pi_family_contract_child(
    state: &Path,
    phase: &str,
    profile: &str,
    pi_override: Option<&Path>,
) -> Result<()> {
    // Each invocation models a watcher restart under a different live policy.
    // Child-only environment changes cannot race other connector tests.
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command
        .args([
            "--exact",
            "indexer::watch_source_ledger::boundary_tests::gh574_watch_pi_family_contract_child",
            "--nocapture",
        ])
        .current_dir(state)
        .env(PI_FAMILY_CONTRACT_CHILD, phase)
        .env(PI_FAMILY_CONTRACT_STATE, state)
        .env("PI_CODING_AGENT_SESSION_DIR", state.join("launch-sessions"))
        .env("OMP_PROFILE", profile)
        .env("RUST_MIN_STACK", "134217728");
    for key in [
        "PI_SESSIONS_DIR",
        "PI_CODING_AGENT_DIR",
        "PI_CONFIG_DIR",
        "PI_PROFILE",
        "CASS_OMP_DATA_ROOT",
        "CASS_EXCLUDE_PATHS",
        "CASS_EXCLUDE_WORKSPACES",
        "CASS_SKIP_SUBAGENTS",
    ] {
        command.env_remove(key);
    }
    if let Some(path) = pi_override {
        command.env("PI_SESSIONS_DIR", path);
    }
    let output = command.output()?;
    let diagnostic = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        output.status.success(),
        "watch contract phase {phase:?}, profile {profile:?} failed: {diagnostic}",
    );
    assert!(
        diagnostic.contains("watch-pi-family-contract-ok"),
        "watch contract child did not execute: {diagnostic}",
    );
    Ok(())
}

#[test]
fn gh574_watch_receipts_track_live_pi_family_ownership_and_profile() -> Result<()> {
    let temp = TempDir::new()?;
    let path = write_pi_family_contract_session(temp.path())?;
    let bytes = fs::read(&path)?;
    run_pi_family_contract_child(temp.path(), "create", "work", None)?;
    run_pi_family_contract_child(temp.path(), "reuse", "work", None)?;
    run_pi_family_contract_child(temp.path(), "reparse", "review", None)?;
    run_pi_family_contract_child(temp.path(), "reuse", "review", None)?;
    // Returning to an earlier profile must replace the latest archive parse,
    // rather than finding a separate receipt under the old profile's key.
    run_pi_family_contract_child(temp.path(), "reparse", "work", None)?;
    let other_pi_store = temp.path().join("other-pi-store");
    fs::create_dir(&other_pi_store)?;
    run_pi_family_contract_child(temp.path(), "reparse", "work", Some(&other_pi_store))?;
    run_pi_family_contract_child(temp.path(), "reuse", "work", Some(&other_pi_store))?;
    assert_eq!(fs::read(path)?, bytes, "only parsing policy changed");
    Ok(())
}

#[cfg(unix)]
#[test]
fn gh574_watch_uncacheable_pi_family_policy_keeps_sources_eligible() -> Result<()> {
    use std::os::unix::{ffi::OsStringExt, fs::symlink};

    let temp = TempDir::new()?;
    write_pi_family_contract_session(temp.path())?;
    run_pi_family_contract_child(temp.path(), "create", "work", None)?;
    // A Unicode override can resolve through an alias to a native path which
    // JSON cannot represent. The actual session remains Unicode and parsable.
    let native = temp
        .path()
        .join(std::ffi::OsString::from_vec(b"pi-\xff-store".to_vec()));
    fs::create_dir(&native)?;
    let alias = temp.path().join("pi-alias");
    symlink(&native, &alias)?;
    run_pi_family_contract_child(temp.path(), "uncacheable", "work", Some(&alias))
}

#[test]
fn gh574_watch_pi_family_contract_child() -> Result<()> {
    let Ok(phase) = dotenvy::var(PI_FAMILY_CONTRACT_CHILD) else {
        return Ok(());
    };
    let state = PathBuf::from(dotenvy::var(PI_FAMILY_CONTRACT_STATE)?);
    let sessions = state.join("launch-sessions");
    let path = sessions.join("2026-08-24T12-00-00_watch.jsonl");
    let saved_path = state.join("source-receipts.json");
    let mut saved: HashMap<String, String> = if phase == "create" {
        HashMap::new()
    } else {
        serde_json::from_slice(&fs::read(&saved_path)?)?
    };
    let (producer_contract, cacheable) = source_ledger_producer_contract("omp")?;
    if phase == "uncacheable" {
        assert!(!cacheable);
        assert!(source_ledger_producer_contract("codex")?.1);
        // Even a legacy receipt that matches the base contract must not be
        // reused when the Pi-family context cannot be encoded faithfully.
        for observation in saved.values_mut() {
            let mut value: serde_json::Value = serde_json::from_str(observation)?;
            value["producer_contract"] = serde_json::json!(producer_contract);
            *observation = value.to_string();
        }
    } else {
        assert!(cacheable);
    }
    let ctx = ScanContext::with_roots(
        state.join("cass-state"),
        vec![ScanRoot::local(sessions)],
        Some(i64::MAX),
    );
    let filter = ActiveSessionSourceFilter::with_recent_write_window_for_test(None);
    let mut ledger = WatchSourceLedger::default();
    let mut delivered = Vec::new();
    ledger.scan(
        &crate::connectors::omp::OmpConnector::new(),
        "omp",
        &ctx,
        &saved,
        true,
        true,
        &filter,
        None,
        &mut |conversation| {
            delivered.push(conversation);
            Ok(true)
        },
    )?;
    if phase == "reuse" {
        assert!(delivered.is_empty());
        assert_eq!(ledger.reused_sources, vec![path]);
        assert!(ledger.completions.is_empty());
    } else {
        assert_eq!(
            delivered.len(),
            1,
            "the unchanged transcript must be reparsed"
        );
        assert_eq!(delivered[0].agent_slug, "omp");
        assert_eq!(delivered[0].source_path, path);
        assert_eq!(
            delivered[0].metadata["profile"],
            dotenvy::var("OMP_PROFILE")?
        );
        assert_eq!(delivered[0].messages[0].content, "keep the latest profile");
        assert!(ledger.reused_sources.is_empty());
        if phase == "uncacheable" {
            assert!(ledger.completions.is_empty());
            assert_eq!(ledger.admitted_keys, saved.keys().cloned().collect());
        } else {
            assert_eq!(ledger.completions.len(), 1);
            let completion = &ledger.completions[&path];
            let value: serde_json::Value = serde_json::from_str(&completion.entry.observation)?;
            assert_eq!(value["producer_contract"], producer_contract);
            assert_eq!(completion.remaining, 1);
            if phase == "reparse" {
                assert_eq!(saved.len(), 1);
                assert_ne!(
                    saved.get(&completion.entry.key),
                    Some(&completion.entry.observation),
                    "the same source key must replace its obsolete interpretation",
                );
                assert!(saved.contains_key(&completion.entry.key));
            }
            saved.insert(
                completion.entry.key.clone(),
                completion.entry.observation.clone(),
            );
            fs::write(saved_path, serde_json::to_vec(&saved)?)?;
        }
    }
    println!("watch-pi-family-contract-ok");
    Ok(())
}
