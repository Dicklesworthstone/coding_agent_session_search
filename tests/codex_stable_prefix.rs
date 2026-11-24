//! Real pinned FAD + CASS regression matrix for growing Codex rollouts.
//!
//! Writer ownership matches GH #559's still/reopen/held arms. Progress ticks
//! coordinate actual writer threads, making appends during admission/parsing
//! deterministic instead of depending on machine speed or a sleep winning a
//! race. These tests exercise the public connector; archive, raw-mirror and
//! durable-publication assertions belong to their respective integration tests.
#![cfg(unix)]

use anyhow::{Context, Result};
use coding_agent_search::connectors::{Connector, ScanContext, ScanRoot, codex::CodexConnector};
use franken_agent_detection::{NormalizedConversation, SourceCompletion, SourceScanHooks};
use serde_json::{Value, json};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

fn message(text: &str) -> Vec<u8> {
    format!(
        "{}\n",
        json!({"timestamp":"2026-11-23T12:00:01.000Z","type":"response_item",
            "payload":{"type":"message","role":"assistant",
                "content":[{"type":"output_text","text":text}]}})
    )
    .into_bytes()
}

fn body(padding: usize) -> Vec<u8> {
    let mut bytes = format!(
        "{}\n",
        json!({"timestamp":"2026-11-23T12:00:00.000Z","type":"session_meta",
            "payload":{"id":"stable-prefix-matrix","cwd":"/synthetic/prefix"}})
    )
    .into_bytes();
    bytes.extend(message("before-marker"));
    // Both genuine parsers ignore these records. Enough lines and bytes cross
    // their heartbeat strides without constructing huge searchable messages.
    for _ in 0..padding {
        bytes.extend_from_slice(b"{\"type\":\"fixture_padding\",\"padding\":\"abcdefghijklmnopqrstuvwxyz0123456789\"}\n");
    }
    bytes
}

struct Fixture {
    _directory: tempfile::TempDir,
    path: PathBuf,
    sibling: PathBuf,
    ctx: ScanContext,
}

fn fixture(padding: usize) -> Result<Fixture> {
    let directory = tempfile::tempdir()?;
    let codex = directory.path().join(".codex");
    let sessions = codex.join("sessions/2026/11/23");
    fs::create_dir_all(&sessions)?;
    let path = sessions.join("rollout-a-growing.jsonl");
    let sibling = sessions.join("rollout-z-finished.jsonl");
    fs::write(&path, body(padding))?;
    fs::write(&sibling, message("healthy-sibling"))?;
    let ctx = ScanContext::with_roots(
        directory.path().join("archive"),
        vec![ScanRoot::local(codex)],
        None,
    );
    Ok(Fixture {
        _directory: directory,
        path,
        sibling,
        ctx,
    })
}

#[derive(Default)]
struct Observed {
    conversations: Vec<NormalizedConversation>,
    completions: Vec<SourceCompletion>,
}

impl Observed {
    fn conversation(&self, path: &Path) -> &NormalizedConversation {
        let matches: Vec<_> = self
            .conversations
            .iter()
            .filter(|conversation| conversation.source_path == path)
            .collect();
        assert_eq!(matches.len(), 1, "one projection per physical source");
        matches[0]
    }

    fn completion(&self, path: &Path) -> &SourceCompletion {
        let matches: Vec<_> = self
            .completions
            .iter()
            .filter(|completion| completion.source.source_path == path)
            .collect();
        assert_eq!(matches.len(), 1, "one publication boundary per source");
        matches[0]
    }
}

fn scan(ctx: &ScanContext, watched: &Path, admitted: &AtomicBool) -> (Result<()>, Observed) {
    let mut observed = Observed::default();
    let mut before = |source: &franken_agent_detection::DiscoveredSourceFile| {
        admitted.store(source.source_path == watched, Ordering::SeqCst);
        true
    };
    let mut complete = |completion: &SourceCompletion| {
        admitted.store(false, Ordering::SeqCst);
        observed.completions.push(completion.clone());
        Ok(())
    };
    let result = CodexConnector::new().scan_with_source_boundaries(
        ctx,
        &mut SourceScanHooks {
            should_scan_source: Some(&mut before),
            on_source_complete: Some(&mut complete),
        },
        &mut |conversation| {
            observed.conversations.push(conversation);
            Ok(())
        },
    );
    admitted.store(false, Ordering::SeqCst);
    (result, observed)
}

fn content(conversation: &NormalizedConversation) -> Vec<&str> {
    for (index, message) in conversation.messages.iter().enumerate() {
        assert_eq!(message.idx, i64::try_from(index).unwrap());
    }
    conversation
        .messages
        .iter()
        .map(|message| message.content.as_str())
        .collect()
}

fn receipt(conversation: &NormalizedConversation) -> &Value {
    conversation
        .metadata
        .pointer("/cass/codex_stable_prefix")
        .expect("delivered prefix must carry byte-boundary evidence")
}

fn assert_partial(completion: &SourceCompletion) {
    assert_eq!(completion.conversations_emitted, 1);
    // The CASS-aware sink consumes the scoped prefix evidence. A generic FAD
    // sink must never mistake that event for an ordinary reusable EOF receipt.
    assert_eq!(completion.source.size_bytes, None);
    assert_eq!(completion.source.modified_at_ms, None);
}

struct Appender {
    command: mpsc::SyncSender<bool>,
    acknowledgement: Mutex<mpsc::Receiver<io::Result<()>>>,
    writes: AtomicUsize,
}

impl Appender {
    fn tick(&self) {
        self.command.send(true).expect("request concurrent append");
        self.acknowledgement
            .lock()
            .unwrap()
            .recv()
            .expect("writer acknowledged append")
            .expect("append synthetic record");
        self.writes.fetch_add(1, Ordering::SeqCst);
    }
}

fn appender(path: &Path, held: bool) -> Result<(Arc<Appender>, JoinHandle<()>)> {
    let (command, requests) = mpsc::sync_channel(0);
    let (acknowledge, acknowledgement) = mpsc::sync_channel(0);
    let (ready, ready_receiver) = mpsc::sync_channel(0);
    let path = path.to_path_buf();
    let thread = std::thread::spawn(move || {
        let mut writer = held.then(|| OpenOptions::new().append(true).open(&path).unwrap());
        ready.send(()).unwrap();
        let mut count = 0;
        while let Ok(true) = requests.recv() {
            let bytes = message(&format!("live-append-{count:06}"));
            let result = match writer.as_mut() {
                Some(writer) => writer.write_all(&bytes).and_then(|()| writer.flush()),
                None => OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .and_then(|mut writer| writer.write_all(&bytes)),
            };
            acknowledge.send(result).unwrap();
            count += 1;
        }
    });
    ready_receiver.recv().context("start writer thread")?;
    Ok((
        Arc::new(Appender {
            command,
            acknowledgement: Mutex::new(acknowledgement),
            writes: AtomicUsize::new(0),
        }),
        thread,
    ))
}

#[test]
fn gh559_still_reopen_and_held_writers_publish_prefix_then_find_every_append() -> Result<()> {
    for arm in ["still", "reopen", "held"] {
        let fixture = fixture(4096)?;
        let (result, first) = scan(&fixture.ctx, &fixture.path, &AtomicBool::new(false));
        result?;
        let original_id = first.conversation(&fixture.path).external_id.clone();
        assert_eq!(
            content(first.conversation(&fixture.path)),
            ["before-marker"]
        );
        let original = fs::read(&fixture.path)?;
        let admitted = Arc::new(AtomicBool::new(false));
        let mut context = fixture.ctx.clone();
        let mut writer = None;
        let expected_appends;
        if arm == "still" {
            let mut file = OpenOptions::new().append(true).open(&fixture.path)?;
            for index in 0..4 {
                file.write_all(&message(&format!("live-append-{index:06}")))?;
            }
            drop(file);
            // Reuse the existing inactivity policy without sleeping or altering
            // it. No writer remains and this complete generation has settled.
            File::open(&fixture.path)?
                .set_modified(SystemTime::now() - Duration::from_secs(3600))?;
            expected_appends = 4;
        } else {
            let (client, handle) = appender(&fixture.path, arm == "held")?;
            let active = Arc::clone(&admitted);
            let tick_writer = Arc::clone(&client);
            context = context.with_progress_tick(Arc::new(move || {
                if active.load(Ordering::SeqCst) {
                    tick_writer.tick();
                }
            }));
            writer = Some((client, handle));
            expected_appends = 0;
        }
        let (result, second) = scan(&context, &fixture.path, &admitted);
        let appends = if let Some((client, handle)) = writer {
            client.command.send(false)?;
            handle.join().expect("join controlled writer");
            let writes = client.writes.load(Ordering::SeqCst);
            assert!(writes > 0, "{arm}: appends must overlap the scan");
            writes
        } else {
            expected_appends
        };
        result?;
        let conversation = second.conversation(&fixture.path);
        assert_eq!(conversation.external_id, original_id, "{arm}");
        assert!(
            !serde_json::to_string(conversation)?.contains("cass-codex-prefix-"),
            "private parser paths must not escape into indexed provenance"
        );
        assert_eq!(
            content(second.conversation(&fixture.sibling)),
            ["healthy-sibling"]
        );
        if arm == "still" {
            assert_eq!(conversation.messages.len(), appends + 1);
            assert_eq!(receipt(conversation)["partial"], false);
            assert_eq!(
                second.completion(&fixture.path).source.size_bytes,
                Some(fs::metadata(&fixture.path)?.len())
            );
        } else {
            assert_eq!(
                content(conversation),
                ["before-marker"],
                "{arm}: later bytes must wait"
            );
            let evidence = receipt(conversation);
            assert_eq!(evidence["partial"], true);
            assert_eq!(evidence["admitted_bytes"], original.len() as u64);
            assert_eq!(evidence["committed_bytes"], original.len() as u64);
            assert_eq!(
                evidence["blake3"],
                blake3::hash(&original).to_hex().to_string()
            );
            assert_partial(second.completion(&fixture.path));
        }
        File::open(&fixture.path)?.set_modified(SystemTime::now() - Duration::from_secs(3600))?;
        let live_bytes = fs::read(&fixture.path)?;
        assert_eq!(&live_bytes[..original.len()], original.as_slice());
        let (result, third) = scan(&fixture.ctx, &fixture.path, &AtomicBool::new(false));
        result?;
        let final_conversation = third.conversation(&fixture.path);
        assert_eq!(final_conversation.external_id, original_id);
        let expected: Vec<_> = std::iter::once("before-marker".to_owned())
            .chain((0..appends).map(|index| format!("live-append-{index:06}")))
            .collect();
        assert_eq!(content(final_conversation), expected);
        assert_eq!(
            third.completion(&fixture.path).source.size_bytes,
            Some(live_bytes.len() as u64)
        );
        assert_eq!(receipt(final_conversation)["partial"], false);
        let (result, repeated) = scan(&fixture.ctx, &fixture.path, &AtomicBool::new(false));
        result?;
        assert_eq!(content(repeated.conversation(&fixture.path)), expected);
        assert_eq!(
            fs::read(&fixture.path)?,
            live_bytes,
            "the connector must not change its source"
        );
    }
    Ok(())
}

#[test]
fn active_cut_json_utf8_and_valid_json_without_lf_wait_for_the_same_record() -> Result<()> {
    let complete_tail = message("tail-marker-🦀");
    let crab = complete_tail
        .windows(4)
        .position(|bytes| bytes == "🦀".as_bytes())
        .unwrap();
    for cut in [complete_tail.len() / 2, crab + 2, complete_tail.len() - 1] {
        let fixture = fixture(0)?;
        let prefix = fs::read(&fixture.path)?;
        let mut writer = OpenOptions::new().append(true).open(&fixture.path)?;
        writer.write_all(&complete_tail[..cut])?;
        writer.flush()?;
        let before = fs::read(&fixture.path)?;
        let (result, first) = scan(&fixture.ctx, &fixture.path, &AtomicBool::new(false));
        result?;
        let conversation = first.conversation(&fixture.path);
        assert_eq!(content(conversation), ["before-marker"]);
        assert_eq!(
            receipt(conversation)["committed_bytes"],
            prefix.len() as u64
        );
        assert_eq!(receipt(conversation)["admitted_bytes"], before.len() as u64);
        assert_partial(first.completion(&fixture.path));
        assert_eq!(fs::read(&fixture.path)?, before);
        writer.write_all(&complete_tail[cut..])?;
        writer.flush()?;
        drop(writer);
        let (result, finished) = scan(&fixture.ctx, &fixture.path, &AtomicBool::new(false));
        result?;
        assert_eq!(
            content(finished.conversation(&fixture.path)),
            ["before-marker", "tail-marker-🦀"]
        );
        assert_eq!(
            finished.conversation(&fixture.path).external_id,
            conversation.external_id
        );
    }
    Ok(())
}

#[test]
fn gh560_abandoned_cut_tail_keeps_its_existing_inactive_recovery() -> Result<()> {
    let fixture = fixture(0)?;
    let mut writer = OpenOptions::new().append(true).open(&fixture.path)?;
    writer.write_all(b"{\"type\":\"response_item\",\"payload\":")?;
    drop(writer);
    File::open(&fixture.path)?.set_modified(SystemTime::now() - Duration::from_secs(7200))?;
    let before = fs::read(&fixture.path)?;
    for _ in 0..2 {
        let (result, observed) = scan(&fixture.ctx, &fixture.path, &AtomicBool::new(false));
        result?;
        assert_eq!(
            content(observed.conversation(&fixture.path)),
            ["before-marker"]
        );
        assert_eq!(
            observed.completion(&fixture.path).source.size_bytes,
            Some(before.len() as u64)
        );
        assert_eq!(fs::read(&fixture.path)?, before);
    }
    Ok(())
}

#[test]
fn truncation_replacement_and_rewrite_plus_append_cannot_publish_old_prefix() -> Result<()> {
    for mutation in ["truncate", "replace", "rewrite-and-grow"] {
        let fixture = fixture(4096)?;
        let original = fs::read(&fixture.path)?;
        let modified = fs::metadata(&fixture.path)?.modified()?;
        let path = fixture.path.clone();
        let admitted = Arc::new(AtomicBool::new(false));
        let fired = Arc::new(AtomicBool::new(false));
        let active = Arc::clone(&admitted);
        let once = Arc::clone(&fired);
        let mut replacement = original.clone();
        let index = replacement
            .windows(b"before-marker".len())
            .position(|bytes| bytes == b"before-marker")
            .unwrap();
        replacement[index..index + b"before-marker".len()].copy_from_slice(b"after--marker");
        let ctx = fixture.ctx.clone().with_progress_tick(Arc::new(move || {
            if !active.load(Ordering::SeqCst) || once.swap(true, Ordering::SeqCst) {
                return;
            }
            match mutation {
                "truncate" => OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_len(8)
                    .unwrap(),
                "replace" => {
                    fs::rename(&path, path.with_extension("retained")).unwrap();
                    fs::write(&path, &replacement).unwrap();
                    File::open(&path).unwrap().set_modified(modified).unwrap();
                }
                "rewrite-and-grow" => {
                    let mut writer = OpenOptions::new().write(true).open(&path).unwrap();
                    writer.write_all(&replacement).unwrap();
                    writer.write_all(&message("post-rewrite-append")).unwrap();
                    writer.set_modified(modified).unwrap();
                }
                _ => unreachable!(),
            }
        }));
        let (result, observed) = scan(&ctx, &fixture.path, &admitted);
        assert!(
            fired.load(Ordering::SeqCst),
            "{mutation}: race was exercised"
        );
        let error = result.expect_err("a changed admitted generation cannot be published");
        assert!(!format!("{error:#}").contains("before-marker"));
        assert!(
            !observed
                .conversations
                .iter()
                .any(|conversation| conversation.source_path == fixture.path)
        );
        assert!(
            !observed
                .completions
                .iter()
                .any(|completion| completion.source.source_path == fixture.path)
        );
        assert_eq!(
            content(observed.conversation(&fixture.sibling)),
            ["healthy-sibling"]
        );
        // A standalone connector can attempt the replacement afresh. A host
        // with an old durable prefix additionally checks its stored overlap.
        fs::write(&fixture.path, &original)?;
        let (result, retry) = scan(&fixture.ctx, &fixture.path, &AtomicBool::new(false));
        result?;
        assert_eq!(
            content(retry.conversation(&fixture.path)),
            ["before-marker"]
        );
    }
    Ok(())
}

#[test]
fn appending_from_delivery_cannot_turn_prefix_publication_into_eof_completion() -> Result<()> {
    let fixture = fixture(0)?;
    File::open(&fixture.path)?.set_modified(SystemTime::now() - Duration::from_secs(3600))?;
    let mut completions = Vec::new();
    let mut complete = |completion: &SourceCompletion| {
        completions.push(completion.clone());
        Ok(())
    };
    CodexConnector::new().scan_with_source_boundaries(
        &fixture.ctx,
        &mut SourceScanHooks {
            should_scan_source: None,
            on_source_complete: Some(&mut complete),
        },
        &mut |conversation| {
            if conversation.source_path == fixture.path {
                assert_eq!(content(&conversation), ["before-marker"]);
                OpenOptions::new()
                    .append(true)
                    .open(&fixture.path)?
                    .write_all(&message("after-delivery"))?;
            }
            Ok(())
        },
    )?;
    let partial = completions
        .iter()
        .find(|completion| completion.source.source_path == fixture.path)
        .unwrap();
    assert_partial(partial);
    let (result, next) = scan(&fixture.ctx, &fixture.path, &AtomicBool::new(false));
    result?;
    assert_eq!(
        content(next.conversation(&fixture.path)),
        ["before-marker", "after-delivery"]
    );
    Ok(())
}

#[test]
fn consumer_failure_prevents_completion_and_replay_keeps_the_same_identity() -> Result<()> {
    let fixture = fixture(0)?;
    let mut emitted_id = None;
    let mut completed = 0;
    let mut complete = |_: &SourceCompletion| {
        completed += 1;
        Ok(())
    };
    let error = CodexConnector::new()
        .scan_with_source_boundaries(
            &fixture.ctx,
            &mut SourceScanHooks {
                should_scan_source: None,
                on_source_complete: Some(&mut complete),
            },
            &mut |conversation| {
                emitted_id = conversation.external_id;
                anyhow::bail!("synthetic durable writer failure")
            },
        )
        .unwrap_err();
    assert!(format!("{error:#}").contains("synthetic durable writer failure"));
    assert_eq!(completed, 0);
    let (result, retry) = scan(&fixture.ctx, &fixture.path, &AtomicBool::new(false));
    result?;
    assert_eq!(retry.conversation(&fixture.path).external_id, emitted_id);
    assert_eq!(
        content(retry.conversation(&fixture.path)),
        ["before-marker"]
    );
    Ok(())
}
