//! Real FAD scan + CASS enrichment, without models or an authoritative archive.
use anyhow::Result;
use coding_agent_search::connectors::{Connector, ScanContext, ScanRoot, codex::CodexConnector};
use franken_agent_detection::connectors::{SourceCompletion, SourceScanHooks};
use serde_json::json;
use std::io::Write;
use std::path::{Path, PathBuf};

const LIMIT: u64 = 100 * 1024 * 1024;

fn fixture(root: &Path) -> Result<(ScanContext, [PathBuf; 3])> {
    let sessions = root.join(".codex/sessions");
    std::fs::create_dir_all(&sessions)?;
    let paths = ["a", "b", "c"].map(|name| sessions.join(format!("rollout-{name}.jsonl")));
    for (i, path) in paths.iter().enumerate() {
        let row = json!({"type":"response_item","payload":{
            "type":"message","role":"user","content":[{"type":"input_text","text":format!("containmentmarker{i}")}]
        }});
        std::fs::write(path, format!("{row}\n"))?;
    }
    Ok((
        ScanContext::with_roots(
            root.join("archive"),
            vec![ScanRoot::local(root.join(".codex"))],
            None,
        ),
        paths,
    ))
}

#[test]
fn oversized_rollout_does_not_hide_later_sources_or_complete_it() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (ctx, paths) = fixture(dir.path())?;
    // All lines are valid JSON, and the first record is a real conversation.
    // Padding is never read by the corrected preflight. This is not a malformed
    // file test masquerading as a size rejection.
    let mut file = std::fs::OpenOptions::new().append(true).open(&paths[1])?;
    let padding = format!(
        "{{\"type\":\"fixture_padding\",\"padding\":\"{}\"}}\n",
        "x".repeat(65536)
    );
    while file.metadata()?.len() <= LIMIT {
        file.write_all(padding.as_bytes())?;
    }
    drop(file);
    let before = paths
        .iter()
        .map(std::fs::read)
        .collect::<std::io::Result<Vec<_>>>()?;
    let mut delivered = Vec::new();
    let mut completed = Vec::new();
    let mut completion = |done: &SourceCompletion| {
        completed.push(done.source.source_path.clone());
        Ok(())
    };
    let mut hooks = SourceScanHooks {
        should_scan_source: None,
        on_source_complete: Some(&mut completion),
    };
    let error = CodexConnector::new()
        .scan_with_source_boundaries(&ctx, &mut hooks, &mut |conversation| {
            delivered.push(conversation.source_path);
            Ok(())
        })
        .unwrap_err();
    assert_eq!(
        delivered,
        [paths[0].clone(), paths[2].clone()],
        "an oversized middle source must not hide the last source"
    );
    assert_eq!(
        completed, delivered,
        "a discarded source must never receive completion"
    );
    let error = error.to_string();
    assert!(error.contains("enrichment_read_budget_exceeded"), "{error}");
    assert!(error.contains("rollout-b.jsonl"), "{error}");
    // Reuse is the host's decision: completed neighbors can be skipped while B
    // remains a reported rejection, rather than becoming an ordinary skip.
    let mut scanned = Vec::new();
    let mut predicate = |source: &coding_agent_search::connectors::DiscoveredSourceFile| {
        scanned.push(source.source_path.clone());
        !completed.contains(&source.source_path)
    };
    let mut repeated = 0;
    let mut hooks = SourceScanHooks {
        should_scan_source: Some(&mut predicate),
        on_source_complete: None,
    };
    assert!(
        CodexConnector::new()
            .scan_with_source_boundaries(&ctx, &mut hooks, &mut |_| {
                repeated += 1;
                Ok(())
            })
            .is_err()
    );
    assert_eq!(scanned, paths);
    assert_eq!(repeated, 0);
    for (path, original) in paths.iter().zip(before) {
        assert_eq!(std::fs::read(path)?, original);
    }
    Ok(())
}

#[test]
fn callback_reports_partial_coverage_and_all_or_error_scan_does_not_claim_success() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (ctx, paths) = fixture(dir.path())?;
    std::fs::OpenOptions::new()
        .write(true)
        .open(&paths[1])?
        .set_len(LIMIT + 1)?;
    let mut emitted = Vec::new();
    assert!(
        CodexConnector::new()
            .scan_with_callback(&ctx, &mut |conversation| {
                emitted.push(conversation.source_path);
                Ok(())
            })
            .is_err()
    );
    assert_eq!(emitted, [paths[0].clone(), paths[2].clone()]);
    assert!(CodexConnector::new().scan(&ctx).is_err());
    Ok(())
}

#[test]
fn intentionally_excluded_source_is_not_reported_as_an_attempted_failure() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (ctx, paths) = fixture(dir.path())?;
    std::fs::OpenOptions::new()
        .write(true)
        .open(&paths[1])?
        .set_len(LIMIT + 1)?;
    let mut predicate = |source: &coding_agent_search::connectors::DiscoveredSourceFile| {
        source.source_path != paths[1]
    };
    let mut hooks = SourceScanHooks {
        should_scan_source: Some(&mut predicate),
        on_source_complete: None,
    };
    let mut emitted = Vec::new();
    CodexConnector::new().scan_with_source_boundaries(&ctx, &mut hooks, &mut |conversation| {
        emitted.push(conversation.source_path);
        Ok(())
    })?;
    assert_eq!(emitted, [paths[0].clone(), paths[2].clone()]);
    Ok(())
}
