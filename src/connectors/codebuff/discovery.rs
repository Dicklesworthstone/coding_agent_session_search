//! Isolate explicitly selected stores without replacing FAD's discovery rules.

use std::collections::HashSet;

use super::scan_failures::ScanFailures;
use crate::connectors::{DiscoveredSourceFile, ScanContext};

pub(super) fn discover_for_scan(
    ctx: &ScanContext,
    failures: &mut ScanFailures,
    mut discover: impl FnMut(&ScanContext) -> anyhow::Result<Vec<DiscoveredSourceFile>>,
) -> anyhow::Result<Vec<DiscoveredSourceFile>> {
    if ctx.use_default_detection() {
        // Default discovery is still FAD's single home-relative store. Never
        // manufacture another root, or turn a failed inventory into success.
        return discover(ctx);
    }

    let mut sources = Vec::new();
    let mut seen = HashSet::new();
    // Clone the context once: cloning all selected roots inside the loop
    // would make a many-file watch selection quadratic before discovery.
    let mut scoped = ctx.clone();
    scoped.scan_roots = Vec::with_capacity(1);
    for root in &ctx.scan_roots {
        scoped.scan_roots.clear();
        scoped.scan_roots.push(root.clone());
        // A targeted metadata/chat selector requests reconstruction of that
        // one chat. A deleted sidecar has no mtime, and copied/repaired state
        // can keep an old one; neither may hide the unchanged primary behind
        // the watch watermark. Reset per root so broad/primary selections
        // still honor their own cutoff. Durable source admission stays later.
        scoped.since_ts = ctx.since_ts;
        if scoped.since_ts.is_some()
            && super::watch_scope::transcript_selector(&root.path) != root.path
        {
            scoped.since_ts = None;
        }
        match discover(&scoped) {
            Ok(found) => {
                for source in found {
                    // Match FAD's physical-store deduplication across selectors,
                    // but not across independent source origins. Preserve the
                    // first selected root's provenance and workspace mappings.
                    let identity = std::fs::canonicalize(&source.source_path)
                        .unwrap_or_else(|_| source.source_path.clone());
                    let key = (
                        source.origin.source_id.clone(),
                        identity,
                        std::mem::discriminant(&source.role),
                    );
                    if seen.insert(key) {
                        sources.push(source);
                    }
                }
            }
            Err(error) => failures.record(root.path.clone(), error),
        }
    }
    sources.sort_by(|left, right| left.source_path.cmp(&right.source_path));
    Ok(sources)
}

/// The indexer inventories inputs before invoking the parser so it can mirror
/// known files and capture reconstruction dependencies. This is a preparatory
/// inventory, not proof that every selected store was scanned successfully.
/// Keep known inputs when another selected root fails; the actual scan visits
/// every root again and remains authoritative for the run's error result.
pub(super) fn discover_for_inventory(
    ctx: &ScanContext,
    discover: impl FnMut(&ScanContext) -> anyhow::Result<Vec<DiscoveredSourceFile>>,
) -> anyhow::Result<Vec<DiscoveredSourceFile>> {
    let mut failures = ScanFailures::default();
    let sources = discover_for_scan(ctx, &mut failures, discover)?;
    if let Err(error) = failures.finish() {
        if sources.is_empty() {
            // No known input is not a successful empty inventory when any
            // selected root failed. Default discovery also fails above.
            return Err(error);
        }
        tracing::warn!(
            error = %error,
            known_sources = sources.len(),
            "partial Codebuff source inventory; the scan will retry failed roots"
        );
    }
    Ok(sources)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::codebuff::CodebuffConnector;
    use crate::connectors::codex::path_policy::ScanExclusions;
    use crate::connectors::{Connector, ScanRoot};
    use serde_json::json;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn fixture(home: &Path, marker: &str) -> PathBuf {
        let chat = home.join("projects/probe/chats/2026-03-21T17-14-03.768Z");
        fs::create_dir_all(&chat).unwrap();
        let primary = chat.join("chat-messages.json");
        fs::write(
            &primary,
            serde_json::to_vec(&json!([{
                "id":"user-1774113351457", "variant":"user", "content":marker,
                "timestamp":"01:15 PM"
            }]))
            .unwrap(),
        )
        .unwrap();
        fs::write(chat.join("run-state.json"), b"{}").unwrap();
        primary
    }

    #[test]
    fn gh511_one_selected_discovery_failure_does_not_discard_other_stores() {
        let temp = tempfile::tempdir().unwrap();
        let primary = fixture(&temp.path().join("healthy"), "healthyrootproof");
        let healthy = primary.ancestors().nth(4).unwrap().to_path_buf();
        let denied = temp.path().join("denied/projects");
        let other_denied = temp.path().join("another-denied/projects");
        let connector = CodebuffConnector::new();
        let exclusions = ScanExclusions::from_env();
        for roots in [
            vec![denied.clone(), healthy.clone(), other_denied.clone()],
            vec![healthy.clone(), denied.clone(), other_denied.clone()],
        ] {
            let ctx = ScanContext::with_roots(
                temp.path().join("data"),
                roots.iter().cloned().map(ScanRoot::local).collect(),
                None,
            );
            let mut failures = ScanFailures::default();
            let mut visited = Vec::new();
            let sources = discover_for_scan(&ctx, &mut failures, |scope| {
                assert_eq!(scope.scan_roots.len(), 1);
                let path = &scope.scan_roots[0].path;
                visited.push(path.clone());
                if path == &denied || path == &other_denied {
                    // Inject only the discovery error at the actual production
                    // scope boundary. Healthy discovery and parsing use FAD.
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "fixture store enumeration denied",
                    )
                    .into());
                }
                connector.discover_allowed(scope, &exclusions)
            })
            .unwrap();
            assert_eq!(visited, roots);
            assert_eq!(sources.len(), 2);
            assert_eq!(sources[0].source_path, primary);
            assert!(sources.iter().all(|source| source.scan_root == healthy));
            let single = CodebuffConnector::source_context(&ctx, &sources[0]);
            let conversations = franken_agent_detection::CodebuffConnector::new()
                .scan(&single)
                .unwrap();
            assert_eq!(conversations.len(), 1);
            assert_eq!(conversations[0].messages[0].content, "healthyrootproof");
            assert_eq!(
                conversations[0].messages[0].created_at,
                Some(1_774_113_351_457)
            );
            let error = failures.finish().unwrap_err();
            let text = error.to_string();
            assert!(text.contains(denied.to_string_lossy().as_ref()));
            assert!(text.contains(other_denied.to_string_lossy().as_ref()));
            assert!(!text.contains(healthy.to_string_lossy().as_ref()));
            assert!(error.chain().any(|cause| cause.is::<std::io::Error>()));
        }
    }

    #[test]
    fn gh511_scoped_discovery_matches_fad_for_overlapping_healthy_selectors() {
        let temp = tempfile::tempdir().unwrap();
        let first = fixture(&temp.path().join("first"), "firstrootproof");
        let second = fixture(&temp.path().join("second"), "secondrootproof");
        let connector = CodebuffConnector::new();
        let exclusions = ScanExclusions::from_env();
        let ctx = ScanContext::with_roots(
            temp.path().join("data"),
            vec![
                ScanRoot::local(first.with_file_name("run-state.json")),
                ScanRoot::local(first.ancestors().nth(4).unwrap().to_path_buf()),
                ScanRoot::local(second.clone()),
                ScanRoot::local(second.parent().unwrap().to_path_buf()),
            ],
            None,
        );
        // The comparator is the original one-call FAD inventory, not the
        // public entry point that now uses the same per-root recovery helper.
        let expected = connector.discover_allowed(&ctx, &exclusions).unwrap();
        let mut failures = ScanFailures::default();
        let found = discover_for_scan(&ctx, &mut failures, |scope| {
            connector.discover_allowed(scope, &exclusions)
        })
        .unwrap();
        failures.finish().unwrap();
        assert_eq!(found, expected);
        assert_eq!(found.len(), 4);
        assert!(
            found
                .iter()
                .filter(|source| source.source_path == first)
                .all(|source| source.scan_root == first.with_file_name("run-state.json"))
        );
        assert_eq!(connector.scan(&ctx).unwrap().len(), 2);
    }

    #[test]
    fn gh511_default_discovery_failure_is_not_relabelled_as_an_empty_scan() {
        let ctx = ScanContext::with_roots(PathBuf::from("unused-data"), Vec::new(), None);
        assert!(ctx.use_default_detection());
        let mut failures = ScanFailures::default();
        let mut calls = 0;
        let error = discover_for_scan(&ctx, &mut failures, |scope| {
            calls += 1;
            assert!(scope.use_default_detection());
            Err(std::io::Error::other("default discovery sentinel").into())
        })
        .unwrap_err();
        assert_eq!(calls, 1);
        assert_eq!(error.to_string(), "default discovery sentinel");
        failures.finish().unwrap();
    }

    #[test]
    fn gh511_preparse_inventory_retains_known_inputs_without_certifying_an_empty_failure() {
        let temp = tempfile::tempdir().unwrap();
        let primary = fixture(&temp.path().join("healthy"), "preparsehealthyproof");
        let healthy = primary.ancestors().nth(4).unwrap().to_path_buf();
        let denied = temp.path().join("denied/projects");
        let connector = CodebuffConnector::new();
        let exclusions = ScanExclusions::from_env();
        let ctx = ScanContext::with_roots(
            temp.path().join("data"),
            vec![ScanRoot::local(denied.clone()), ScanRoot::local(healthy)],
            None,
        );
        let mut discover = |scope: &ScanContext| {
            if scope.scan_roots[0].path == denied {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "preparse permission sentinel",
                )
                .into())
            } else {
                connector.discover_allowed(scope, &exclusions)
            }
        };
        // The host can snapshot/mirror the healthy reconstruction unit before
        // calling scan_with_source_boundaries. Previously its inventory ?
        // stopped the pipeline before healthy callbacks could be reached.
        let known = discover_for_inventory(&ctx, &mut discover).unwrap();
        assert_eq!(known.len(), 2);
        assert_eq!(known[0].source_path, primary);
        assert_eq!(known[1].source_path, primary.with_file_name("run-state.json"));
        let mut failures = ScanFailures::default();
        let scanned = discover_for_scan(&ctx, &mut failures, &mut discover).unwrap();
        assert_eq!(known, scanned);
        let error = failures.finish().unwrap_err();
        assert!(error.to_string().contains("preparse permission sentinel"));
        assert!(error.chain().any(|cause| cause.is::<std::io::Error>()));

        let mut failed_only = ctx;
        failed_only.scan_roots.truncate(1);
        let error = discover_for_inventory(&failed_only, &mut discover).unwrap_err();
        assert!(error.to_string().contains("preparse permission sentinel"));
    }
}

#[cfg(test)]
mod replay_tests;
