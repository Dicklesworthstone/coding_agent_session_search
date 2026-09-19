use super::*;
use crate::connectors::codex::CodexConnector;
use std::fs;
use std::path::PathBuf;

pub(super) fn message(text: &str) -> String {
    format!(
        "{}\n",
        serde_json::json!({"type":"response_item","payload":{"role":"user","content":text}})
    )
}

pub(super) fn corpus() -> Result<(tempfile::TempDir, Vec<PathBuf>, ScanContext)> {
    let root = tempfile::tempdir()?;
    let sessions = root.path().join(".codex/sessions/2026/09/19");
    fs::create_dir_all(&sessions)?;
    let paths: Vec<_> = ["a", "b", "c"]
        .iter()
        .map(|name| sessions.join(format!("rollout-{name}.jsonl")))
        .collect();
    for (index, path) in paths.iter().enumerate() {
        fs::write(path, message(&format!("healthy-{index}")))?;
    }
    let ctx = ScanContext::with_roots(
        root.path().join("cass"),
        vec![ScanRoot::local(root.path().join(".codex"))],
        None,
    );
    Ok((root, paths, ctx))
}

#[test]
fn unfinished_middle_source_does_not_starve_later_sessions() -> Result<()> {
    let (_root, paths, mut ctx) = corpus()?;
    let broken = format!("{}{{\"type\":", message("unfinished-private-prefix"));
    fs::write(&paths[1], &broken)?;
    let connector = CodexConnector::new();
    for since in [None, Some(0)] {
        ctx.since_ts = since;
        let inventory = connector.discover_source_files(&ctx)?;
        let mut completed = Vec::new();
        let mut visited = Vec::new();
        let mut predicate = |source: &DiscoveredSourceFile| {
            assert!(inventory.contains(source), "pre-parse identity changed");
            visited.push(source.source_path.clone());
            true
        };
        let mut complete = |done: &SourceCompletion| {
            assert!(
                inventory.contains(&done.source),
                "completion identity changed"
            );
            completed.push(done.source.source_path.clone());
            Ok(())
        };
        let mut hooks = SourceScanHooks {
            should_scan_source: Some(&mut predicate),
            on_source_complete: Some(&mut complete),
        };
        let mut delivered = Vec::new();
        let error = connector
            .scan_with_source_boundaries(&ctx, &mut hooks, &mut |conversation| {
                delivered.push(conversation.source_path);
                Ok(())
            })
            .unwrap_err();
        assert_eq!(visited, paths);
        assert_eq!(delivered, [paths[0].clone(), paths[2].clone()]);
        assert_eq!(completed, delivered);
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::UnexpectedEof
        );
        let report = error.downcast_ref::<FailureReport>().unwrap();
        assert_eq!(report.failed_source_count, 1);
        assert_eq!(report.failed_sources[0].reason, "unfinished_source");
        assert_eq!(PathBuf::from(&report.failed_sources[0].source_path), paths[1]);
        assert!(!error.to_string().contains("unfinished-private-prefix"));
        assert!(
            connector.scan(&ctx).is_err(),
            "collected scans cannot return partial success"
        );
        assert_eq!(fs::read_to_string(&paths[1])?, broken);
    }
    fs::write(&paths[1], message("repaired"))?;
    let retry = connector.scan(&ctx)?;
    assert_eq!(retry.len(), 3);
    assert_eq!(retry[1].messages[0].content, "repaired");
    assert_eq!(
        retry[1].external_id.as_deref().unwrap().replace('\\', "/"),
        "2026/09/19/rollout-b"
    );
    Ok(())
}

#[test]
fn invalid_data_and_budget_failures_are_both_reported() -> Result<()> {
    let (_root, paths, ctx) = corpus()?;
    let mut invalid = message("private-data").into_bytes();
    invalid.extend_from_slice(b"\xff\n");
    fs::write(&paths[0], &invalid)?;
    fs::File::create(&paths[1])?.set_len(super::super::MAX_AUGMENT_ROLLOUT_BYTES + 1)?;
    let mut delivered = Vec::new();
    let error = CodexConnector::new()
        .scan_with_callback(&ctx, &mut |conversation| {
            delivered.push(conversation.source_path);
            Ok(())
        })
        .unwrap_err();
    assert_eq!(delivered, [paths[2].clone()]);
    let report = error.downcast_ref::<FailureReport>().unwrap();
    assert_eq!(report.failed_source_count, 2);
    assert_eq!(report.failed_sources[0].reason, "invalid_source_data");
    assert_eq!(report.failed_sources[1].reason, "read_budget_exceeded");
    assert_eq!(
        error.downcast_ref::<io::Error>().unwrap().kind(),
        io::ErrorKind::InvalidData
    );
    assert_eq!(fs::read(&paths[0])?, invalid);
    Ok(())
}

#[test]
fn consumer_errors_abort_even_after_a_source_local_failure() -> Result<()> {
    for completion_error in [false, true] {
        let (_root, paths, ctx) = corpus()?;
        fs::write(&paths[0], format!("{}{{", message("unfinished")))?;
        let mut delivered = Vec::new();
        let mut complete = |_: &SourceCompletion| -> Result<()> {
            if completion_error {
                return Err(
                    io::Error::new(io::ErrorKind::UnexpectedEof, "consumer failure").into()
                );
            }
            Ok(())
        };
        let mut hooks = SourceScanHooks {
            should_scan_source: None,
            on_source_complete: Some(&mut complete),
        };
        let error = CodexConnector::new()
            .scan_with_source_boundaries(&ctx, &mut hooks, &mut |conversation| {
                delivered.push(conversation.source_path);
                if !completion_error {
                    return Err(
                        io::Error::new(io::ErrorKind::UnexpectedEof, "consumer failure").into()
                    );
                }
                Ok(())
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "consumer failure");
        assert!(error.downcast_ref::<FailureReport>().is_none());
        assert_eq!(
            delivered,
            [paths[1].clone()],
            "later storage writes must stop"
        );
    }
    Ok(())
}

#[test]
fn disappeared_inventory_source_is_not_a_successful_empty_scan() -> Result<()> {
    let (_root, paths, ctx) = corpus()?;
    let mut complete = |done: &SourceCompletion| -> Result<()> {
        if done.source.source_path == paths[0] {
            fs::rename(&paths[1], paths[1].with_extension("retained"))?;
        }
        Ok(())
    };
    let mut hooks = SourceScanHooks {
        should_scan_source: None,
        on_source_complete: Some(&mut complete),
    };
    let mut delivered = Vec::new();
    let error = CodexConnector::new()
        .scan_with_source_boundaries(&ctx, &mut hooks, &mut |conversation| {
            delivered.push(conversation.source_path);
            Ok(())
        })
        .unwrap_err();
    assert_eq!(delivered, [paths[0].clone(), paths[2].clone()]);
    assert_eq!(
        error.downcast_ref::<io::Error>().unwrap().kind(),
        io::ErrorKind::NotFound
    );
    assert_eq!(
        error
            .downcast_ref::<FailureReport>()
            .unwrap()
            .failed_source_count,
        1
    );
    Ok(())
}
