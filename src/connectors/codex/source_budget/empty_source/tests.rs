use super::*;
use crate::connectors::codex::CodexConnector;
use franken_agent_detection::{
    Connector, ScanContext, ScanRoot, SourceCompletion, SourceScanHooks,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

fn message(text: &str) -> String {
    format!(
        "{}\n",
        serde_json::json!({"type":"response_item","payload":{"role":"user","content":text}})
    )
}

fn fixture(
    extension: &str,
    bytes: &[u8],
) -> Result<(tempfile::TempDir, Vec<PathBuf>, ScanContext)> {
    let temp = tempfile::tempdir()?;
    // FAD sorts discovered paths, even with explicit roots. Keep the failed
    // source in the middle so the ordered assertions prove later-source work.
    let paths = vec![
        temp.path().join("rollout-a-before.jsonl"),
        temp.path().join(format!("rollout-b-empty.{extension}")),
        temp.path().join("rollout-c-after.jsonl"),
    ];
    fs::write(&paths[0], message("before"))?;
    fs::write(&paths[1], bytes)?;
    fs::write(&paths[2], message("after"))?;
    let ctx = ScanContext::with_roots(
        temp.path().join("cass"),
        paths.iter().cloned().map(ScanRoot::local).collect(),
        None,
    );
    Ok((temp, paths, ctx))
}

#[test]
fn zero_message_failures_do_not_report_success_or_starve_healthy_sources() -> Result<()> {
    let cases: &[(&str, &[u8], io::ErrorKind)] = &[
        ("json", b"", io::ErrorKind::UnexpectedEof),
        ("json", b"{\"items\":[", io::ErrorKind::UnexpectedEof),
        ("json", b"{\"items\":oops}", io::ErrorKind::InvalidData),
        (
            "json",
            b"{\"items\":[]} trailing",
            io::ErrorKind::InvalidData,
        ),
        (
            "json",
            b"{\"items\":[\"\xff\"]}",
            io::ErrorKind::InvalidData,
        ),
        ("jsonl", b"{\"type\":", io::ErrorKind::UnexpectedEof),
        ("jsonl", b"\xff\n", io::ErrorKind::InvalidData),
        (
            "jsonl",
            b"{\"type\":\"session_meta\",\"payload\":{}}\n{",
            io::ErrorKind::UnexpectedEof,
        ),
    ];
    for &(extension, bytes, expected) in cases {
        let (_temp, paths, mut ctx) = fixture(extension, bytes)?;
        for since in [None, Some(0)] {
            ctx.since_ts = since;
            let mut delivered = Vec::new();
            let mut completed = Vec::new();
            let mut complete = |done: &SourceCompletion| {
                // ubs:ignore[rust.collections.clone-in-loop] -- Own each source path retained by the test callback.
                completed.push(done.source.source_path.clone());
                Ok(())
            };
            let mut hooks = SourceScanHooks {
                should_scan_source: None,
                on_source_complete: Some(&mut complete),
            };
            let connector = CodexConnector::new();
            let error = connector
                .scan_with_source_boundaries(&ctx, &mut hooks, &mut |conversation| {
                    delivered.push(conversation.source_path);
                    Ok(())
                })
                .unwrap_err();
            // ubs:ignore[rust.panic.assert-macros] -- Test failure must reject the incorrect error kind.
            assert_eq!(error.downcast_ref::<io::Error>().unwrap().kind(), expected);
            // ubs:ignore[rust.panic.assert-macros,rust.collections.clone-in-loop] -- Compare owned fixture paths; mismatch must fail the test.
            assert_eq!(delivered, [paths[0].clone(), paths[2].clone()]);
            // ubs:ignore[rust.panic.assert-macros] -- Test assertion forbids certifying an undelivered source.
            assert_eq!(completed, delivered);
            // ubs:ignore[rust.panic.assert-macros] -- A corrupt source must fail the collecting scan too.
            assert!(connector.scan(&ctx).is_err());
            // ubs:ignore[rust.panic.assert-macros] -- The scan must preserve the fixture's source bytes.
            assert_eq!(fs::read(&paths[1])?, bytes);
        }
        let repaired = if extension == "json" {
            // ubs:ignore[rust.perf.string-alloc-in-loop] -- Own one small repaired fixture for this test case.
            "{\"items\":[{\"role\":\"user\",\"content\":\"repaired\"}]}".to_owned()
        } else {
            message("repaired")
        };
        fs::write(&paths[1], repaired)?;
        let retry = CodexConnector::new().scan(&ctx)?;
        // ubs:ignore[rust.panic.assert-macros] -- Retry must recover all three fixture conversations.
        assert_eq!(retry.len(), 3);
        // ubs:ignore[rust.panic.assert-macros] -- The repaired source must supply its actual message.
        assert_eq!(retry[1].messages[0].content, "repaired");
    }
    Ok(())
}

#[test]
fn valid_empty_sources_and_historical_jsonl_tolerance_are_preserved() -> Result<()> {
    for (extension, content) in [
        ("json", "{\"items\":[]}"),
        ("json", "\u{feff}{\"items\":[],\"session\":{}}\r\n"),
        ("jsonl", ""),
        ("jsonl", " \r\n\n"),
        ("jsonl", "{\"type\":\"session_meta\",\"payload\":{}}"),
        (
            "jsonl",
            "\u{feff}{\"type\":\"session_meta\",\"payload\":{}}\r\nnot-json\n",
        ),
    ] {
        let (_temp, paths, ctx) = fixture(extension, content.as_bytes())?;
        let conversations = CodexConnector::new().scan(&ctx)?;
        // ubs:ignore[rust.panic.assert-macros] -- Empty input must not fabricate a conversation.
        assert_eq!(conversations.len(), 2, "{extension}");
        // ubs:ignore[rust.panic.assert-macros] -- Keep the healthy source preceding the empty fixture.
        assert_eq!(conversations[0].source_path, paths[0]);
        // ubs:ignore[rust.panic.assert-macros] -- Keep the healthy source following the empty fixture.
        assert_eq!(conversations[1].source_path, paths[2]);
        // ubs:ignore[rust.panic.assert-macros] -- Scanning must not mutate the source fixture.
        assert_eq!(fs::read_to_string(&paths[1])?, content);
    }
    Ok(())
}

#[test]
fn compressed_zero_message_sources_validate_decoded_text_and_refuse_damage() -> Result<()> {
    let text = b"{\"type\":\"session_meta\",\"payload\":{}}\n";
    let compressed = zstd::stream::encode_all(text.as_slice(), 0)?;
    let (_temp, paths, ctx) = fixture("jsonl.zst", &compressed)?;
    let conversations = CodexConnector::new().scan(&ctx)?;
    // ubs:ignore[rust.panic.assert-macros] -- Preserve the original reporter's two-conversation regression.
    assert_eq!(conversations.len(), 2);
    // ubs:ignore[rust.panic.assert-macros] -- The preceding healthy fixture must remain indexed.
    assert_eq!(conversations[0].source_path, paths[0]);
    // ubs:ignore[rust.panic.assert-macros] -- The following healthy fixture must remain indexed.
    assert_eq!(conversations[1].source_path, paths[2]);
    // ubs:ignore[rust.panic.assert-macros] -- The compressed fixture must remain byte-identical.
    assert_eq!(fs::read(&paths[1])?, compressed);

    for damaged in [
        compressed[..compressed.len() - 1].to_vec(),
        zstd::stream::encode_all(b"\xff\n".as_slice(), 0)?,
        zstd::stream::encode_all(b"{\"type\":".as_slice(), 0)?,
    ] {
        fs::write(&paths[1], &damaged)?;
        // ubs:ignore[rust.panic.assert-macros] -- Damage is expected to fail this connector regression.
        assert!(CodexConnector::new().scan(&ctx).is_err());
        // ubs:ignore[rust.panic.assert-macros] -- Failure must not repair or overwrite the source fixture.
        assert_eq!(fs::read(&paths[1])?, damaged);
    }
    let large = zstd::stream::encode_all(" \n".repeat(4096).as_bytes(), 0)?;
    fs::write(&paths[1], large)?;
    // ubs:ignore[rust.panic.assert-macros] -- The regression must fail if decoded input escapes its byte limit.
    assert!(validate_with_limit(&paths[1], None, 4096).is_err());
    Ok(())
}

#[test]
fn compressed_empty_sources_succeed_without_fabricating_conversations() -> Result<()> {
    let text = b"{\"type\":\"session_meta\",\"payload\":{}}\n";
    let frame = zstd::stream::encode_all(text.as_slice(), 0)?;
    // A frame boundary can divide a JSON record, not just separate lines.
    let mut split_record = zstd::bulk::compress(&text[..17], 0)?;
    split_record.extend(zstd::stream::encode_all(&text[17..], 0)?);
    for compressed in [
        frame.clone(),
        zstd::bulk::compress(text, 0)?,
        zstd::stream::encode_all(b"".as_slice(), 0)?,
        zstd::stream::encode_all(
            "\u{feff}{\"type\":\"session_meta\"}\r\nnot-json\n".as_bytes(),
            0,
        )?,
        [frame.as_slice(), frame.as_slice()].concat(),
        split_record,
    ] {
        let (_temp, paths, mut ctx) = fixture("jsonl.zst", &compressed)?;
        let originals = paths.iter().map(fs::read).collect::<io::Result<Vec<_>>>()?;
        for since in [None, Some(0)] {
            ctx.since_ts = since;
            let mut completed = Vec::new();
            let mut complete = |done: &SourceCompletion| {
                // ubs:ignore[rust.collections.clone-in-loop] -- Retain an owned completion observation for this test case.
                completed.push((done.source.source_path.clone(), done.conversations_emitted));
                Ok(())
            };
            let mut hooks = SourceScanHooks {
                should_scan_source: None,
                on_source_complete: Some(&mut complete),
            };
            let mut delivered = Vec::new();
            CodexConnector::new().scan_with_source_boundaries(
                &ctx,
                &mut hooks,
                // ubs:ignore[rust.panic.assert-macros] -- This synchronous test callback verifies each healthy message.
                &mut |conversation| {
                    let expected_content = if conversation.source_path == paths[0] {
                        "before"
                    } else {
                        "after"
                    };
                    assert_eq!(conversation.messages.len(), 1);
                    assert_eq!(conversation.messages[0].content, expected_content);
                    delivered.push(conversation.source_path);
                    Ok(())
                },
            )?;
            // ubs:ignore[rust.panic.assert-macros,rust.collections.clone-in-loop] -- Compare owned fixture paths; mismatch must fail the test.
            assert_eq!(delivered, [paths[0].clone(), paths[2].clone()]);
            // FAD only emits a completion when it emits a conversation.
            // Preserve that contract; validating empty input must synthesize
            // neither a conversation nor an upstream completion event.
            // ubs:ignore[rust.panic.assert-macros,rust.collections.clone-in-loop] -- Require exactly the healthy fixtures' completion records.
            assert_eq!(completed, [(paths[0].clone(), 1), (paths[2].clone(), 1)]);
            // ubs:ignore[rust.panic.assert-macros] -- All source fixtures must survive indexing byte-for-byte.
            assert_eq!(
                paths.iter().map(fs::read).collect::<io::Result<Vec<_>>>()?,
                originals
            );
        }
    }
    Ok(())
}

#[test]
fn compressed_source_failures_withhold_completion_and_keep_both_neighbors() -> Result<()> {
    let text = b"{\"type\":\"session_meta\",\"payload\":{}}\n";
    let frame = zstd::stream::encode_all(text.as_slice(), 0)?;
    let mut checksum_encoder = zstd::stream::Encoder::new(Vec::new(), 0)?;
    checksum_encoder.include_checksum(true)?;
    std::io::Write::write_all(&mut checksum_encoder, text)?;
    let mut bad_checksum = checksum_encoder.finish()?;
    *bad_checksum.last_mut().unwrap() ^= 1;
    for (damaged, expected) in [
        (Vec::new(), io::ErrorKind::UnexpectedEof),
        (
            frame[..frame.len() - 1].to_vec(),
            io::ErrorKind::UnexpectedEof,
        ),
        (
            [frame.as_slice(), &frame[..frame.len() - 1]].concat(),
            io::ErrorKind::UnexpectedEof,
        ),
        (
            [frame.as_slice(), bad_checksum.as_slice()].concat(),
            io::ErrorKind::Other,
        ),
        (bad_checksum, io::ErrorKind::Other),
        (text.to_vec(), io::ErrorKind::Other),
        (
            [frame.as_slice(), b"not a frame".as_slice()].concat(),
            io::ErrorKind::Other,
        ),
        (
            zstd::stream::encode_all(b"\xff\n".as_slice(), 0)?,
            io::ErrorKind::InvalidData,
        ),
        (
            zstd::stream::encode_all(b"{\"private_marker\":".as_slice(), 0)?,
            io::ErrorKind::UnexpectedEof,
        ),
    ] {
        let (_temp, paths, mut ctx) = fixture("jsonl.zst", &damaged)?;
        let originals = paths.iter().map(fs::read).collect::<io::Result<Vec<_>>>()?;
        // Primary parsing rejects some damage before the zero-output path.
        // Exercise that fallback directly so upstream validation cannot mask
        // an incomplete-frame or decoded-text regression in this validator.
        let validation_error = validate(&paths[1], None).unwrap_err();
        // ubs:ignore[rust.panic.assert-macros] -- Fallback validation must independently report the expected corruption kind.
        assert_eq!(
            validation_error.downcast_ref::<io::Error>().unwrap().kind(),
            expected,
            "{validation_error:#}"
        );
        for since in [None, Some(0)] {
            ctx.since_ts = since;
            let mut completed = Vec::new();
            let mut complete = |done: &SourceCompletion| {
                // ubs:ignore[rust.collections.clone-in-loop] -- Own each source path retained by the test callback.
                completed.push(done.source.source_path.clone());
                Ok(())
            };
            let mut hooks = SourceScanHooks {
                should_scan_source: None,
                on_source_complete: Some(&mut complete),
            };
            let mut delivered = Vec::new();
            let error = CodexConnector::new()
                // ubs:ignore[rust.panic.assert-macros] -- This synchronous test callback verifies healthy messages around corrupted input.
                .scan_with_source_boundaries(&ctx, &mut hooks, &mut |conversation| {
                    let expected_content = if conversation.source_path == paths[0] {
                        "before"
                    } else {
                        "after"
                    };
                    assert_eq!(conversation.messages.len(), 1);
                    assert_eq!(conversation.messages[0].content, expected_content);
                    delivered.push(conversation.source_path);
                    Ok(())
                })
                .unwrap_err();
            // ubs:ignore[rust.panic.assert-macros] -- Corruption must retain its actionable I/O error kind.
            assert_eq!(
                error.downcast_ref::<io::Error>().unwrap().kind(),
                expected,
                "{error:#}"
            );
            // ubs:ignore[rust.perf.string-alloc-in-loop] -- Materialize each fixture's error chain for diagnostic assertions.
            let diagnostic = format!("{error:#}");
            let serialized_path = serde_json::to_string(&paths[1].to_string_lossy())?;
            // ubs:ignore[rust.panic.assert-macros] -- The diagnostic must identify the failed physical fixture.
            assert!(diagnostic.contains(&serialized_path), "{diagnostic}");
            let reason = match expected {
                io::ErrorKind::UnexpectedEof => "unfinished_source",
                io::ErrorKind::InvalidData => "invalid_source_data",
                _ => "source_read_failed",
            };
            // ubs:ignore[rust.panic.assert-macros] -- The fixture's error must carry its expected recovery reason.
            assert!(diagnostic.contains(reason), "{diagnostic}");
            // ubs:ignore[rust.panic.assert-macros] -- Diagnostics must never disclose the fixture's private-content marker.
            assert!(!diagnostic.contains("private_marker"), "{diagnostic}");
            // ubs:ignore[rust.panic.assert-macros,rust.collections.clone-in-loop] -- Compare owned fixture paths; mismatch must fail the test.
            assert_eq!(delivered, [paths[0].clone(), paths[2].clone()]);
            // ubs:ignore[rust.panic.assert-macros] -- Failed-source completions are forbidden by this regression.
            assert_eq!(completed, delivered);
            // ubs:ignore[rust.panic.assert-macros] -- All three source fixtures must remain unchanged after failure.
            assert_eq!(
                paths.iter().map(fs::read).collect::<io::Result<Vec<_>>>()?,
                originals
            );
        }
    }
    Ok(())
}

#[test]
fn compressed_zero_output_validation_enforces_exact_decoded_budget() -> Result<()> {
    let text = " \n".repeat(2048);
    let frame = zstd::stream::encode_all(text.as_bytes(), 0)?;
    let declared_frame = zstd::bulk::compress(text.as_bytes(), 0)?;
    let whole = text.repeat(2);
    let limit = 2 * text.len() as u64;
    for compressed in [
        [frame.as_slice(), frame.as_slice()].concat(),
        [declared_frame.as_slice(), declared_frame.as_slice()].concat(),
        zstd::stream::encode_all(whole.as_bytes(), 0)?,
        zstd::bulk::compress(whole.as_bytes(), 0)?,
    ] {
        let (_temp, paths, _ctx) = fixture("jsonl.zst", &compressed)?;
        // ubs:ignore[rust.panic.assert-macros] -- This fixture must isolate the decoded limit from the physical-file limit.
        assert!((compressed.len() as u64) < limit - 1);
        validate_with_limit(&paths[1], None, limit)?;
        let error = validate_with_limit(&paths[1], None, limit - 1).unwrap_err();
        // ubs:ignore[rust.panic.assert-macros] -- One byte over the decoded budget must fail, never appear as clean EOF.
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::FileTooLarge
        );
        // ubs:ignore[rust.panic.assert-macros,rust.perf.string-alloc-in-loop] -- Assert the configured byte limit in this test's error text.
        assert!(error.to_string().contains(&(limit - 1).to_string()));
        // ubs:ignore[rust.panic.assert-macros] -- Budget rejection must leave the compressed fixture intact.
        assert_eq!(fs::read(&paths[1])?, compressed);
    }
    Ok(())
}

#[test]
fn compressed_zero_output_validation_rechecks_its_opened_snapshot() -> Result<()> {
    use std::io::Write;

    let frame = zstd::stream::encode_all(b"{}\n".as_slice(), 0)?;
    for mutation in ["append", "truncate at frame boundary", "replace"] {
        let compressed = if mutation == "truncate at frame boundary" {
            [frame.as_slice(), frame.as_slice()].concat()
        } else {
            // ubs:ignore[rust.collections.clone-in-loop] -- Each mutation needs its own owned source fixture.
            frame.clone()
        };
        let (_temp, paths, _ctx) = fixture("jsonl.zst", &compressed)?;
        let modified = fs::metadata(&paths[1])?.modified()?;
        let retained = paths[1].with_extension("retained");
        let ticks = AtomicUsize::new(0);
        let tick = || {
            if ticks.fetch_add(1, Ordering::Relaxed) != 0 {
                return;
            }
            match mutation {
                "append" => fs::OpenOptions::new()
                    .append(true)
                    .open(&paths[1])
                    .unwrap()
                    .write_all(&frame)
                    .unwrap(),
                // A complete first frame still decodes to clean EOF, so this
                // must fail on snapshot integrity rather than frame syntax.
                "truncate at frame boundary" => fs::OpenOptions::new()
                    .write(true)
                    .open(&paths[1])
                    .unwrap()
                    .set_len(frame.len() as u64)
                    .unwrap(),
                "replace" => {
                    fs::rename(&paths[1], &retained).unwrap();
                    fs::write(&paths[1], &compressed).unwrap();
                    // Unix must catch a different inode even with identical
                    // length and mtime. Other platforms use observable mtime.
                    let replacement_mtime = if cfg!(unix) {
                        modified
                    } else {
                        modified + std::time::Duration::from_secs(2)
                    };
                    fs::OpenOptions::new()
                        .write(true)
                        .open(&paths[1])
                        .unwrap()
                        .set_modified(replacement_mtime)
                        .unwrap();
                }
                _ => unreachable!(),
            }
        };
        // Decoding the admitted prefix succeeds; the snapshot guard must
        // reject the concurrent mutation before certifying successful input.
        let error = validate(&paths[1], Some(&tick)).unwrap_err();
        // ubs:ignore[rust.panic.assert-macros] -- The mutation fixture must trip the snapshot-integrity guard.
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::Interrupted,
            "{mutation}: {error:#}"
        );
        // ubs:ignore[rust.panic.assert-macros] -- The deterministic callback must mutate the fixture exactly once.
        assert_eq!(ticks.load(Ordering::Relaxed), 1);
        let expected = match mutation {
            "append" => [compressed.as_slice(), frame.as_slice()].concat(),
            // ubs:ignore[rust.collections.clone-in-loop] -- Own the expected bytes of this bounded truncation fixture.
            "truncate at frame boundary" => frame.clone(),
            "replace" => {
                // ubs:ignore[rust.panic.assert-macros] -- The validator must not mutate the retained original inode.
                assert_eq!(fs::read(&retained)?, compressed);
                // ubs:ignore[rust.collections.clone-in-loop] -- Own the expected bytes of the replacement fixture.
                compressed.clone()
            }
            _ => unreachable!(),
        };
        // ubs:ignore[rust.panic.assert-macros] -- Only the fixture callback's intended mutation may be present.
        assert_eq!(fs::read(&paths[1])?, expected, "{mutation}");
        validate(&paths[1], None)?;
    }
    Ok(())
}

#[test]
fn host_veto_prevents_validation_of_intentionally_skipped_input() -> Result<()> {
    for extension in ["json", "jsonl", "jsonl.zst"] {
        let (_temp, paths, ctx) = fixture(extension, b"\xff")?;
        let mut predicate =
            |source: &franken_agent_detection::DiscoveredSourceFile| source.source_path != paths[1];
        let mut hooks = SourceScanHooks {
            should_scan_source: Some(&mut predicate),
            on_source_complete: None,
        };
        let mut delivered = Vec::new();
        CodexConnector::new().scan_with_source_boundaries(
            &ctx,
            &mut hooks,
            &mut |conversation| {
                delivered.push(conversation.source_path);
                Ok(())
            },
        )?;
        // ubs:ignore[rust.panic.assert-macros,rust.collections.clone-in-loop] -- Host veto must preserve exactly the healthy fixture sources.
        assert_eq!(delivered, [paths[0].clone(), paths[2].clone()]);
    }
    Ok(())
}

#[test]
fn broken_archives_with_no_messages_are_not_successful_empty_history() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let home = temp.path().join(".codex");
    let archive = home.join("archived_sessions");
    fs::create_dir_all(&archive)?;
    let path = archive.join("rollout-broken.json");
    fs::write(&path, b"{\"items\":[")?;
    let ctx = ScanContext::with_roots(
        temp.path().join("cass"),
        vec![ScanRoot::local(home)],
        Some(i64::MAX),
    );
    let error = CodexConnector::new().scan(&ctx).unwrap_err();
    // ubs:ignore[rust.panic.assert-macros] -- The deliberately truncated archive fixture must fail as unfinished input.
    assert_eq!(
        error.downcast_ref::<io::Error>().unwrap().kind(),
        io::ErrorKind::UnexpectedEof
    );
    fs::write(&path, "{\"items\":[]}")?;
    // ubs:ignore[rust.panic.assert-macros] -- Repairing an empty archive must not fabricate history.
    assert!(CodexConnector::new().scan(&ctx)?.is_empty());
    Ok(())
}

#[test]
fn zero_output_validation_is_bounded_and_checks_its_opened_snapshot() -> Result<()> {
    let (_temp, paths, _ctx) = fixture("jsonl", b"{\"type\":\"session_meta\"}\n")?;
    let ticks = AtomicUsize::new(0);
    let tick = || {
        if ticks.fetch_add(1, Ordering::Relaxed) == 0 {
            use std::io::Write;
            writeln!(fs::OpenOptions::new().append(true).open(&paths[1]).unwrap()).unwrap();
        }
    };
    let error = validate(&paths[1], Some(&tick)).unwrap_err();
    // ubs:ignore[rust.panic.assert-macros] -- The test callback's append must trip the opened-snapshot guard.
    assert_eq!(
        error.downcast_ref::<io::Error>().unwrap().kind(),
        io::ErrorKind::Interrupted
    );
    fs::File::create(&paths[1])?.set_len(MAX_AUGMENT_ROLLOUT_BYTES + 1)?;
    let error = validate(&paths[1], None).unwrap_err();
    // ubs:ignore[rust.panic.assert-macros] -- The oversized fixture must appear in the rejection count.
    assert_eq!(
        error
            .downcast_ref::<IncompleteScan>()
            .unwrap()
            .rejected_source_count,
        1
    );
    Ok(())
}
