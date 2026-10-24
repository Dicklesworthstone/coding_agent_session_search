//! Exercise the host's live diagnostic entry point with the adapter's typed
//! aggregate. Synthetic I/O causes here are not OS-permission qualification.

use super::{MAX_FAILURE_SAMPLES, ScanFailures};
use crate::connector_ingest_diagnostics::{
    ConnectorIngestRun, IngestFailureKind, IngestSeverity, SourceIngestDisposition,
};
use crate::connectors::codebuff::CodebuffConnector;
use crate::connectors::{
    Connector, DiscoveredSourceFile, DiscoveredSourceRole, NormalizedConversation, ScanContext,
    ScanRoot,
};
use serde_json::json;
use std::path::{Path, PathBuf};

fn context() -> ScanContext {
    ScanContext::with_roots(
        PathBuf::from("cass-data"),
        vec![ScanRoot::local(PathBuf::from("selected-store"))],
        None,
    )
}

fn source(ctx: &ScanContext, path: &Path) -> DiscoveredSourceFile {
    DiscoveredSourceFile::new(
        "codebuff",
        &ctx.scan_roots[0],
        path.to_path_buf(),
        DiscoveredSourceRole::PrimarySessionLog,
        true,
    )
}

fn conversation(path: &Path) -> NormalizedConversation {
    NormalizedConversation {
        agent_slug: "codebuff".into(),
        external_id: Some("observed-conversation".into()),
        title: None,
        workspace: None,
        source_path: path.to_path_buf(),
        started_at: None,
        ended_at: None,
        metadata: json!({}),
        messages: Vec::new(),
    }
}

#[derive(Debug)]
struct OuterScanError(anyhow::Error);

impl std::fmt::Display for OuterScanError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("outer scan wrapper naming a misleading /different/path")
    }
}

impl std::error::Error for OuterScanError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

#[test]
fn gh511_mixed_typed_failures_keep_independent_paths_and_retry_actions_in_every_order() {
    let ctx = context();
    let paths = [
        PathBuf::from("selected-store/busy-permission/chat-messages.json"),
        PathBuf::from("selected-store/unlocked-雪/chat-messages.json"),
        PathBuf::from("selected-store/plain/chat-messages.json"),
    ];
    let healthy = Path::new("selected-store/healthy/chat-messages.json");
    let mut sources: Vec<_> = paths.iter().map(|path| source(&ctx, path)).collect();
    sources.push(source(&ctx, healthy));
    for order in [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ] {
        let mut errors = ScanFailures::default();
        for index in order {
            let error = match index {
                // The I/O kind, not the lock-looking message, owns this cause.
                0 => anyhow::Error::new(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "database is locked: private-payload-sentinel",
                )),
                // A parser quoting a lock is still a parse failure.
                1 => anyhow::Error::new(<serde_json::Error as serde::de::Error>::custom(
                    "database is locked: private-payload-sentinel",
                )),
                _ => anyhow::Error::new(crate::franken_sync::FrankenError::Busy),
            };
            errors.record(paths[index].clone(), error.context("source context"));
        }
        let error = anyhow::Error::new(OuterScanError(
            errors.finish().unwrap_err().context("inner context"),
        ))
        .context("host context");
        let mut run = ConnectorIngestRun::begin("codebuff", &ctx.data_dir, &ctx, &sources);
        run.observe_reused_sources([healthy.to_path_buf()]);
        run.observe_connector_scan_error(&ctx.data_dir, &error);
        let report = run.finish();
        assert_eq!(report.diagnostics.len(), 3, "order={order:?}");
        for (index, kind, severity, retryable, disposition) in [
            (
                0,
                IngestFailureKind::UnreadableSource,
                IngestSeverity::Error,
                true,
                SourceIngestDisposition::Skipped,
            ),
            (
                1,
                IngestFailureKind::UnparseableSource,
                IngestSeverity::Error,
                false,
                SourceIngestDisposition::Skipped,
            ),
            (
                2,
                IngestFailureKind::SourceLocked,
                IngestSeverity::Warning,
                true,
                SourceIngestDisposition::Locked,
            ),
        ] {
            let found = report
                .diagnostics
                .iter()
                .find(|diagnostic| Path::new(&diagnostic.source_path) == paths[index].as_path())
                .unwrap();
            assert_eq!(found.failure_kind, kind, "order={order:?}");
            assert_eq!(found.severity, severity);
            assert_eq!(found.retryable, retryable);
            assert_eq!(found.disposition, disposition);
            assert_eq!(
                found.safe_next_action.contains("check permissions"),
                index == 0
            );
            assert!(
                found.detail.is_none(),
                "never copy arbitrary error payloads into detail"
            );
        }
        assert_eq!(report.summary.indexed, 1);
        assert_eq!(report.summary.skipped, 2);
        assert_eq!(report.summary.locked, 1);
        assert_eq!(report.summary.discovered, 0);
        assert_eq!(
            report.summary.total(),
            4,
            "no synthetic failed fallback root"
        );
        let wire = serde_json::to_string(&report).unwrap();
        assert!(!wire.contains("private-payload-sentinel"));
        assert!(!wire.contains("/different/path"));
    }
}

#[test]
fn gh511_post_delivery_failure_preserves_partial_content_and_healthy_neighbors() {
    let ctx = context();
    let partial = Path::new("selected-store/partial/chat-messages.json");
    let healthy = Path::new("selected-store/healthy/chat-messages.json");
    let bad = Path::new("selected-store/bad/chat-messages.json");
    let sources: Vec<_> = [partial, healthy, bad]
        .into_iter()
        .map(|path| source(&ctx, path))
        .collect();
    let mut run = ConnectorIngestRun::begin("codebuff", &ctx.data_dir, &ctx, &sources);
    run.observe_conversation(&mut conversation(partial));
    run.observe_conversation(&mut conversation(healthy));
    let mut failures = ScanFailures::default();
    failures.record(
        partial.to_path_buf(),
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "post-parse observation failed",
        )
        .into(),
    );
    failures.record(bad.to_path_buf(), anyhow::anyhow!("unsupported transcript"));
    run.observe_connector_scan_error(&ctx.data_dir, &failures.finish().unwrap_err());
    // A later reuse observation cannot erase a previously recorded failure.
    run.observe_reused_sources([partial.to_path_buf(), bad.to_path_buf()]);
    let report = run.finish();
    assert_eq!(report.summary.indexed, 1);
    assert_eq!(report.summary.partially_indexed, 1);
    assert_eq!(report.summary.skipped, 1);
    assert_eq!(report.summary.with_content(), 2);
    assert_eq!(report.summary.total(), 3);
    let diagnostic = report
        .diagnostics
        .iter()
        .find(|diagnostic| Path::new(&diagnostic.source_path) == partial)
        .unwrap();
    assert_eq!(
        diagnostic.disposition,
        SourceIngestDisposition::PartiallyIndexed
    );
    assert_eq!(diagnostic.failure_kind, IngestFailureKind::UnreadableSource);
    assert!(diagnostic.retryable);
}

#[test]
fn gh511_structured_samples_stay_bounded_and_do_not_invent_omitted_source_verdicts() {
    let ctx = context();
    let total = MAX_FAILURE_SAMPLES + 9;
    let mut failures = ScanFailures::default();
    let mut sources = Vec::new();
    for index in 0..total {
        let path = PathBuf::from(format!("selected-store/chat-{index}/chat-messages.json"));
        sources.push(source(&ctx, &path));
        failures.record(
            path,
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied").into(),
        );
    }
    let error = failures.finish().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("9 additional source failures omitted")
    );
    let mut run = ConnectorIngestRun::begin("codebuff", &ctx.data_dir, &ctx, &sources);
    run.observe_connector_scan_error(&ctx.data_dir, &error);
    let report = run.finish();
    assert_eq!(report.diagnostics.len(), MAX_FAILURE_SAMPLES);
    assert_eq!(report.summary.skipped, MAX_FAILURE_SAMPLES as u64);
    assert_eq!(report.summary.discovered, 9);
    assert_eq!(report.summary.with_content(), 0);
    assert_eq!(report.summary.total(), total as u64);
    for (diagnostic, source) in report.diagnostics.iter().zip(&sources) {
        assert_eq!(
            Path::new(&diagnostic.source_path),
            source.source_path.as_path()
        );
        assert_eq!(diagnostic.failure_kind, IngestFailureKind::UnreadableSource);
    }
}

#[test]
fn gh511_untyped_or_foreign_errors_keep_the_existing_fallback_contract() {
    let ctx = context();
    let mut errors = ScanFailures::default();
    errors.record(
        PathBuf::from("not-the-cursor-store"),
        std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied").into(),
    );
    let error = errors.finish().unwrap_err();
    let mut foreign = ConnectorIngestRun::begin("cursor", &ctx.data_dir, &ctx, &[]);
    foreign.observe_connector_scan_error(&ctx.data_dir, &error);
    let report = foreign.finish();
    assert_eq!(report.diagnostics.len(), 1);
    assert_eq!(
        report.diagnostics[0].source_path,
        ctx.data_dir.to_string_lossy()
    );
    assert_eq!(
        report.diagnostics[0].failure_kind,
        IngestFailureKind::UnreadableSource
    );

    for error in [
        anyhow::anyhow!("Codebuff /different/path: invalid transcript"),
        anyhow::Error::new(ScanFailures::default()),
    ] {
        let mut visited = 0;
        assert!(!CodebuffConnector::for_each_source_failure(
            &error,
            |_, _| visited += 1,
        ));
        assert_eq!(visited, 0);
        let mut run = ConnectorIngestRun::begin("codebuff", &ctx.data_dir, &ctx, &[]);
        run.observe_connector_scan_error(&ctx.data_dir, &error);
        let report = run.finish();
        assert_eq!(report.diagnostics.len(), 1);
        assert_eq!(
            report.diagnostics[0].source_path,
            ctx.data_dir.to_string_lossy()
        );
        assert_eq!(
            report.diagnostics[0].failure_kind,
            IngestFailureKind::UnparseableSource
        );
    }
}

#[test]
fn gh511_actual_fad_partial_scan_reports_each_bad_transcript_not_its_common_root() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("projects");
    let mut paths = Vec::new();
    for (project, bytes) in [
        ("a-busy-json", b"[".to_vec()),
        (
            "middle-good",
            serde_json::to_vec(&json!([{
                "id":"user-1774113351457", "variant":"user", "content":"healthyproof",
                "timestamp":"01:15 PM"
            }]))
            .unwrap(),
        ),
        (
            "z-bad-time",
            serde_json::to_vec(&json!([{
                "id":"user-1774113411457", "variant":"ai", "content":"notindexedproof",
                "timestamp":"2026-13-45T99:00:00Z"
            }]))
            .unwrap(),
        ),
    ] {
        let chat = root.join(project).join("chats/2026-03-21T17-14-03.768Z");
        std::fs::create_dir_all(&chat).unwrap();
        let path = chat.join("chat-messages.json");
        std::fs::write(&path, &bytes).unwrap();
        paths.push((path, bytes));
    }
    let ctx = ScanContext::with_roots(temp.path().join("data"), vec![ScanRoot::local(root)], None);
    let connector = CodebuffConnector::new();
    let inventory = connector.discover_source_files(&ctx).unwrap();
    assert_eq!(inventory.len(), 3);
    let mut run = ConnectorIngestRun::begin("codebuff", &ctx.data_dir, &ctx, &inventory);
    let mut delivered = 0;
    let error = connector
        .scan_with_callback(&ctx, &mut |mut conversation| {
            delivered += 1;
            assert_eq!(conversation.messages[0].content, "healthyproof");
            assert_eq!(conversation.messages[0].created_at, Some(1_774_113_351_457));
            run.observe_conversation(&mut conversation);
            Ok(())
        })
        .unwrap_err()
        .context("host scan context");
    run.observe_connector_scan_error(&ctx.data_dir, &error);
    let report = run.finish();
    assert_eq!(delivered, 1);
    assert_eq!(report.summary.indexed, 1);
    assert_eq!(report.summary.skipped, 2);
    assert_eq!(report.summary.discovered, 0);
    assert_eq!(report.summary.total(), 3);
    assert_eq!(report.diagnostics.len(), 2);
    for (diagnostic, index) in report.diagnostics.iter().zip([0, 2]) {
        assert_eq!(Path::new(&diagnostic.source_path), paths[index].0.as_path());
        assert_eq!(
            diagnostic.failure_kind,
            IngestFailureKind::UnparseableSource
        );
        assert_eq!(diagnostic.disposition, SourceIngestDisposition::Skipped);
        assert!(!diagnostic.retryable);
    }
    for (path, before) in paths {
        assert_eq!(std::fs::read(path).unwrap(), before);
    }
}
