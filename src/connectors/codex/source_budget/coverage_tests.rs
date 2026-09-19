//! Source-coverage regressions exercise the real published Codex parser.

use super::*;
use franken_agent_detection::{DetectionResult, ScanRoot};
use std::fs;
use std::io;

fn record(text: &str) -> String {
    serde_json::json!({"role": "user", "content": text}).to_string()
}

fn corpus(extension: &str) -> Result<(tempfile::TempDir, Vec<PathBuf>, ScanContext)> {
    let root = tempfile::tempdir()?;
    let mut paths = Vec::new();
    for name in ["a", "b", "c"] {
        let path = root.path().join(format!("rollout-{name}.{extension}"));
        let message = record(name);
        let body = if extension == "json" {
            format!("{{\"items\":[{message}]}}")
        } else {
            format!("{{\"type\":\"response_item\",\"payload\":{message}}}\n")
        };
        fs::write(&path, body)?;
        paths.push(path);
    }
    let ctx = ScanContext::with_roots(
        root.path().join("cass"),
        paths.iter().cloned().map(ScanRoot::local).collect(),
        None,
    );
    Ok((root, paths, ctx))
}

#[test]
fn oversized_legacy_rollouts_report_partial_coverage_without_hiding_healthy_sources() -> Result<()>
{
    let (_root, paths, ctx) = corpus("json")?;
    let size = MAX_AUGMENT_ROLLOUT_BYTES + 1;
    fs::File::create(&paths[1])?.set_len(size)?;
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
    let connector = super::super::CodexConnector::new();
    let error = connector
        .scan_with_source_boundaries(&ctx, &mut hooks, &mut |conversation| {
            delivered.push(conversation.source_path);
            Ok(())
        })
        .unwrap_err();
    let incomplete = error.downcast_ref::<IncompleteScan>().unwrap();
    assert_eq!(incomplete.rejected_source_count, 1);
    assert_eq!(incomplete.rejected_sources[0].observed_bytes, size);
    assert_eq!(
        incomplete.rejected_sources[0].source_path,
        paths[1].to_string_lossy()
    );
    assert_eq!(delivered, [paths[0].clone(), paths[2].clone()]);
    assert_eq!(completed, delivered);
    assert_eq!(fs::metadata(&paths[1])?.len(), size);
    assert!(connector.scan(&ctx).is_err());

    fs::write(&paths[1], format!("{{\"items\":[{}]}}", record("repaired")))?;
    let retry = connector.scan(&ctx)?;
    assert_eq!(retry.len(), 3);
    assert_eq!(retry[1].messages[0].content, "repaired");
    Ok(())
}

/// Inject a real source rewrite after CASS admits it but before the real
/// parser reads it. Do not mock normalization, discovery or completion logic.
struct RewriteAfterAdmission {
    target: PathBuf,
}

impl Connector for RewriteAfterAdmission {
    fn detect(&self) -> DetectionResult {
        DetectionResult::not_found()
    }

    fn scan(&self, _ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        anyhow::bail!("this fault-injection adapter requires source boundaries")
    }

    fn supports_source_boundaries(&self) -> bool {
        true
    }

    fn scan_with_source_boundaries(
        &self,
        ctx: &ScanContext,
        hooks: &mut SourceScanHooks<'_>,
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        let SourceScanHooks {
            should_scan_source,
            on_source_complete,
        } = hooks;
        let mut predicate = |source: &DiscoveredSourceFile| {
            let admitted = should_scan_source
                .as_mut()
                .is_none_or(|predicate| predicate(source));
            if admitted && source.source_path == self.target {
                fs::write(
                    &self.target,
                    "{\"type\":\"session_meta\",\"payload\":{}}\n\n\n",
                )
                .unwrap();
            }
            admitted
        };
        let mut complete = |done: &SourceCompletion| {
            on_source_complete
                .as_mut()
                .map_or(Ok(()), |sink| sink(done))
        };
        let mut guarded = SourceScanHooks {
            should_scan_source: Some(&mut predicate),
            on_source_complete: Some(&mut complete),
        };
        franken_agent_detection::CodexConnector::new().scan_with_source_boundaries(
            ctx,
            &mut guarded,
            on_conversation,
        )
    }
}

#[test]
fn changed_zero_output_sources_fail_at_next_source_and_end_of_scan() -> Result<()> {
    for last in [false, true] {
        let (_root, paths, mut ctx) = corpus("jsonl")?;
        fs::write(&paths[1], "{\"type\":\"session_meta\",\"payload\":{}}\n")?;
        if last {
            ctx.scan_roots.pop();
        }
        let adapter = RewriteAfterAdmission {
            target: paths[1].clone(),
        };
        let mut delivered = Vec::new();
        let error = scan(
            &adapter,
            &ctx,
            &mut SourceScanHooks::default(),
            &mut |conversation| {
                delivered.push(conversation.source_path);
                Ok(())
            },
            |_| Ok(()),
        )
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().map(io::Error::kind),
            Some(io::ErrorKind::Interrupted),
            "{error:#}"
        );
        assert_eq!(delivered, [paths[0].clone()]);
    }
    Ok(())
}

#[test]
fn stable_empty_and_metadata_only_sources_remain_valid() -> Result<()> {
    for body in ["", " \n\n", "{\"type\":\"session_meta\",\"payload\":{}}\n"] {
        let (_root, paths, ctx) = corpus("jsonl")?;
        fs::write(&paths[1], body)?;
        let conversations = super::super::CodexConnector::new().scan(&ctx)?;
        assert_eq!(conversations.len(), 2);
        assert_eq!(conversations[0].source_path, paths[0]);
        assert_eq!(conversations[1].source_path, paths[2]);
        assert_eq!(fs::read_to_string(&paths[1])?, body);
    }
    Ok(())
}

#[test]
fn later_appends_to_completed_sources_do_not_fail_the_next_source() -> Result<()> {
    let (_root, paths, ctx) = corpus("jsonl")?;
    let mut completed = Vec::new();
    let mut complete = |done: &SourceCompletion| {
        completed.push(done.source.source_path.clone());
        Ok(())
    };
    let mut predicate = |source: &DiscoveredSourceFile| {
        if source.source_path == paths[1] {
            use std::io::Write;
            let mut file = fs::OpenOptions::new().append(true).open(&paths[0]).unwrap();
            writeln!(file).unwrap();
        }
        true
    };
    let mut hooks = SourceScanHooks {
        should_scan_source: Some(&mut predicate),
        on_source_complete: Some(&mut complete),
    };
    let mut delivered = Vec::new();
    super::super::CodexConnector::new().scan_with_source_boundaries(
        &ctx,
        &mut hooks,
        &mut |conversation| {
            delivered.push(conversation.source_path);
            Ok(())
        },
    )?;
    assert_eq!(delivered, paths);
    assert_eq!(completed, delivered);
    Ok(())
}
