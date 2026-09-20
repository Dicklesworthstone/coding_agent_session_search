use super::*;
use crate::connectors::codex::CodexConnector;
use franken_agent_detection::{Connector, ScanContext, ScanRoot, SourceCompletion, SourceScanHooks};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

fn message(text: &str) -> String {
    format!(
        "{}\n",
        serde_json::json!({"type":"response_item","payload":{"role":"user","content":text}})
    )
}

fn fixture(extension: &str, bytes: &[u8]) -> Result<(tempfile::TempDir, Vec<PathBuf>, ScanContext)> {
    let temp = tempfile::tempdir()?;
    let paths = vec![
        temp.path().join("rollout-before.jsonl"),
        temp.path().join(format!("rollout-empty.{extension}")),
        temp.path().join("rollout-after.jsonl"),
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
        ("json", b"{\"items\":[]} trailing", io::ErrorKind::InvalidData),
        ("json", b"{\"items\":[\"\xff\"]}", io::ErrorKind::InvalidData),
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
            assert_eq!(error.downcast_ref::<io::Error>().unwrap().kind(), expected);
            assert_eq!(delivered, [paths[0].clone(), paths[2].clone()]);
            assert_eq!(completed, delivered);
            assert!(connector.scan(&ctx).is_err());
            assert_eq!(fs::read(&paths[1])?, bytes);
        }
        let repaired = if extension == "json" {
            "{\"items\":[{\"role\":\"user\",\"content\":\"repaired\"}]}".to_owned()
        } else {
            message("repaired")
        };
        fs::write(&paths[1], repaired)?;
        let retry = CodexConnector::new().scan(&ctx)?;
        assert_eq!(retry.len(), 3);
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
        ("jsonl", "\u{feff}{\"type\":\"session_meta\",\"payload\":{}}\r\nnot-json\n"),
    ] {
        let (_temp, paths, ctx) = fixture(extension, content.as_bytes())?;
        let conversations = CodexConnector::new().scan(&ctx)?;
        assert_eq!(conversations.len(), 2, "{extension}");
        assert_eq!(conversations[0].source_path, paths[0]);
        assert_eq!(conversations[1].source_path, paths[2]);
        assert_eq!(fs::read_to_string(&paths[1])?, content);
    }
    Ok(())
}

#[test]
fn host_veto_prevents_validation_of_intentionally_skipped_input() -> Result<()> {
    for extension in ["json", "jsonl"] {
        let (_temp, paths, ctx) = fixture(extension, b"\xff")?;
        let mut predicate = |source: &franken_agent_detection::DiscoveredSourceFile| {
            source.source_path != paths[1]
        };
        let mut hooks = SourceScanHooks {
            should_scan_source: Some(&mut predicate),
            on_source_complete: None,
        };
        let mut delivered = Vec::new();
        CodexConnector::new().scan_with_source_boundaries(&ctx, &mut hooks, &mut |conversation| {
            delivered.push(conversation.source_path);
            Ok(())
        })?;
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
    assert_eq!(
        error.downcast_ref::<io::Error>().unwrap().kind(),
        io::ErrorKind::UnexpectedEof
    );
    fs::write(&path, "{\"items\":[]}")?;
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
    assert_eq!(error.downcast_ref::<io::Error>().unwrap().kind(), io::ErrorKind::Interrupted);
    fs::File::create(&paths[1])?.set_len(MAX_AUGMENT_ROLLOUT_BYTES + 1)?;
    let error = validate(&paths[1], None).unwrap_err();
    assert_eq!(error.downcast_ref::<IncompleteScan>().unwrap().rejected_source_count, 1);
    Ok(())
}
