//! Admission mechanics with real files and the existing enrichment parser.
//! No model, private rollout, or archive-scale performance claim is involved.

use super::*;
use std::fs;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};

fn conversation(path: &Path) -> NormalizedConversation {
    NormalizedConversation {
        agent_slug: "codex".into(),
        external_id: Some("limit-fixture".into()),
        title: None,
        workspace: None,
        source_path: path.to_path_buf(),
        started_at: None,
        ended_at: None,
        metadata: serde_json::json!({}),
        messages: Vec::new(),
    }
}

fn tool_record() -> String {
    format!(
        "{}\n",
        serde_json::json!({
            "type": "response_item",
            "timestamp": "2026-09-01T00:00:00Z",
            "payload": {
                "type": "function_call", "name": "exec_command", "call_id": "call-1",
                "arguments": "{\"cmd\":\"cargo test 日本語\"}"
            }
        })
    )
}

#[test]
fn policy_accepts_bounded_decimal_limits_and_keeps_legacy_ceiling() -> Result<()> {
    assert_eq!(ScanLimits::default().jsonl_bytes, 104_857_600);
    for value in ["1", " 536870912 \n", "1073741824"] {
        let policy = ScanLimits::parse(value)?;
        let expected = value.trim().parse::<u64>()?;
        assert_eq!(policy.for_path(Path::new("rollout-test.jsonl")), expected);
        for name in ["rollout-test.json", "rollout-test.JSONL", "rollout-test"] {
            assert_eq!(policy.for_path(Path::new(name)), expected.min(104_857_600));
        }
    }
    Ok(())
}

#[test]
fn malformed_limits_never_become_unlimited_or_leak_the_setting() {
    for value in [
        "",
        " ",
        "0",
        "-1",
        "+1",
        "1.5",
        "NaN",
        "512MiB",
        "1_024",
        "1073741825",
        "18446744073709551616",
        "秘密",
        "\nprivate-value\n",
    ] {
        let error = ScanLimits::parse(value).unwrap_err().to_string();
        assert!(error.contains(SOURCE_LIMIT_ENV));
        if value.trim().len() > 12 {
            assert!(!error.contains(value.trim()));
        }
    }
}

#[test]
fn opened_size_and_zero_output_validator_apply_the_same_exact_budget() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("rollout-empty.jsonl");
    let bytes = b"{\"type\":\"session_meta\",\"payload\":{}}\n";
    fs::write(&path, bytes)?;
    let limit = bytes.len() as u64;
    let snapshot = SourceSnapshot::capture_with_limit(&path, limit)?;
    assert_eq!(snapshot.len(), limit);
    empty_source::validate_with_limit(&path, None, limit)?;
    let error = SourceSnapshot::capture_with_limit(&path, limit - 1)
        .err()
        .expect("one byte over admission limit");
    assert_eq!(
        error
            .downcast_ref::<EnrichmentBudgetExceeded>()
            .unwrap()
            .observed_bytes,
        limit
    );
    let error = empty_source::validate_with_limit(&path, None, limit - 1).unwrap_err();
    let report = error.downcast_ref::<IncompleteScan>().unwrap();
    assert_eq!(report.limit_bytes, limit - 1);
    assert_eq!(report.rejected_source_count, 1);
    assert_eq!(fs::read(&path)?, bytes);
    Ok(())
}

#[test]
fn configured_preflight_refuses_before_parsing_or_completing_a_source() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("rollout-budget.jsonl");
    fs::write(&path, tool_record())?;
    let size = fs::metadata(&path)?.len();
    let ctx = ScanContext::with_roots(
        root.path().join("cass"),
        vec![franken_agent_detection::ScanRoot::local(path.clone())],
        None,
    );
    let admission = ScanAdmission {
        exclusions: ScanExclusions::from_env(),
        limits: ScanLimits {
            jsonl_bytes: size - 1,
        },
    };
    let mut complete = |_: &SourceCompletion| panic!("rejected source cannot complete");
    let mut hooks = SourceScanHooks {
        should_scan_source: None,
        on_source_complete: Some(&mut complete),
    };
    let error = scan_with_admission(
        &franken_agent_detection::CodexConnector::new(),
        &ctx,
        &mut hooks,
        &mut |_| panic!("rejected source cannot emit"),
        &admission,
        |_| panic!("rejected source cannot enrich"),
    )
    .unwrap_err();
    let report = error.downcast_ref::<IncompleteScan>().unwrap();
    assert_eq!(report.limit_bytes, size - 1);
    assert_eq!(report.rejected_sources[0].observed_bytes, size);
    assert!(report.rejected_sources[0].limit_bytes.is_none());
    Ok(())
}

/// One zstd frame holding `len` newlines as a single RLE block: a file of
/// 14 bytes whose header declares `len` bytes of text (GH #513).
fn rle_zstd_frame(len: u32) -> Vec<u8> {
    assert!(len <= 128 * 1024, "one block holds at most 128 KiB");
    let mut frame = vec![0x28, 0xB5, 0x2F, 0xFD]; // magic number
    frame.push(0xA0); // 4-byte content size, single segment
    frame.extend_from_slice(&len.to_le_bytes());
    let block = 1 | (1 << 1) | (len << 3); // last block, RLE, `len` repeats
    frame.extend_from_slice(&block.to_le_bytes()[..3]);
    frame.push(b'\n');
    frame
}

#[test]
fn compressed_rollouts_are_admitted_by_their_declared_text_length() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("rollout-budget.jsonl.zst");
    let text_len: u64 = 100_000;
    fs::write(&path, rle_zstd_frame(text_len as u32))?;
    let file_len = fs::metadata(&path)?.len();
    assert!(file_len < 32, "the file is far smaller than its text");

    // The opened-handle recheck sizes the source by its declared text.
    let error = SourceSnapshot::capture_with_limit(&path, text_len - 1)
        .err()
        .expect("declared text one byte over the limit");
    assert_eq!(
        error
            .downcast_ref::<EnrichmentBudgetExceeded>()
            .unwrap()
            .observed_bytes,
        text_len
    );
    assert!(SourceSnapshot::capture_with_limit(&path, text_len).is_ok());

    // So does the path preflight of a real scan: a typed budget rejection
    // under the JSONL budget, before anything is parsed or completed.
    let ctx = ScanContext::with_roots(
        root.path().join("cass"),
        vec![franken_agent_detection::ScanRoot::local(path.clone())],
        None,
    );
    let admission = ScanAdmission {
        exclusions: ScanExclusions::from_env(),
        limits: ScanLimits {
            jsonl_bytes: text_len - 1,
        },
    };
    assert_eq!(admission.limits.for_path(&path), text_len - 1);
    let mut complete = |_: &SourceCompletion| panic!("rejected source cannot complete");
    let mut hooks = SourceScanHooks {
        should_scan_source: None,
        on_source_complete: Some(&mut complete),
    };
    let error = scan_with_admission(
        &franken_agent_detection::CodexConnector::new(),
        &ctx,
        &mut hooks,
        &mut |_| panic!("rejected source cannot emit"),
        &admission,
        |_| panic!("rejected source cannot enrich"),
    )
    .unwrap_err();
    let report = error.downcast_ref::<IncompleteScan>().unwrap();
    assert_eq!(report.rejected_source_count, 1);
    assert_eq!(report.rejected_sources[0].observed_bytes, text_len);
    assert!(report.rejected_sources[0].limit_bytes.is_none());
    Ok(())
}

#[test]
fn legacy_refusal_records_its_lower_effective_limit() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("rollout-legacy.json");
    fs::File::create(&path)?.set_len(MAX_AUGMENT_ROLLOUT_BYTES + 1)?;
    let limits = ScanLimits {
        jsonl_bytes: 512 * 1024 * 1024,
    };
    let mut state = ScanState {
        limits,
        incomplete: limits.incomplete(),
        ..ScanState::default()
    };
    state.reject(&path, fs::metadata(&path)?.len());
    let report = serde_json::to_value(&state.incomplete)?;
    assert_eq!(report["limit_bytes"], limits.jsonl_bytes);
    assert_eq!(
        report["rejected_sources"][0]["limit_bytes"],
        MAX_AUGMENT_ROLLOUT_BYTES
    );
    assert_eq!(limits.for_path(&path), MAX_AUGMENT_ROLLOUT_BYTES);
    Ok(())
}

#[test]
fn retained_handle_enrichment_reuses_tool_parser_and_rewinds_for_each_call() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("rollout-enrich.jsonl");
    let bytes = tool_record();
    fs::write(&path, &bytes)?;
    let snapshot = SourceSnapshot::capture_with_limit(&path, bytes.len() as u64)?;
    for _ in 0..2 {
        let mut output = conversation(&path);
        snapshot.enrich(&mut output, None)?;
        assert_eq!(output.messages.len(), 1);
        assert!(output.messages[0].content.contains("exec_command"));
        assert_eq!(
            output.messages[0].invocations[0].call_id.as_deref(),
            Some("call-1")
        );
        assert_eq!(
            output.messages[0].invocations[0].arguments,
            Some(serde_json::json!({"cmd": "cargo test 日本語"}))
        );
    }
    assert_eq!(fs::read_to_string(&path)?, bytes);
    Ok(())
}

#[test]
fn retained_prefix_does_not_follow_appends_or_certify_changed_source() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("rollout-changing.jsonl");
    let bytes = tool_record();
    fs::write(&path, &bytes)?;
    let snapshot = SourceSnapshot::capture_with_limit(&path, bytes.len() as u64)?;
    let appended = AtomicBool::new(false);
    let tick = || {
        if !appended.swap(true, Ordering::SeqCst) {
            fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(tool_record().as_bytes())
                .unwrap();
        }
    };
    let error = snapshot
        .enrich(&mut conversation(&path), Some(&tick))
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<io::Error>().unwrap().kind(),
        io::ErrorKind::Interrupted
    );
    assert!(appended.load(Ordering::SeqCst));
    Ok(())
}

#[test]
fn retained_enrichment_refuses_unfinished_tail_and_path_replacement() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("rollout-tail.jsonl");
    fs::write(&path, format!("{}{{\"type\":", tool_record()))?;
    let snapshot = SourceSnapshot::capture_with_limit(&path, 4096)?;
    let error = snapshot.enrich(&mut conversation(&path), None).unwrap_err();
    assert_eq!(
        error.downcast_ref::<io::Error>().unwrap().kind(),
        io::ErrorKind::UnexpectedEof
    );
    fs::rename(&path, root.path().join("retained-source"))?;
    fs::write(&path, b"replacement\n")?;
    let error = snapshot.enrich(&mut conversation(&path), None).unwrap_err();
    assert_eq!(
        error.downcast_ref::<io::Error>().unwrap().kind(),
        io::ErrorKind::Interrupted
    );
    Ok(())
}
