use super::*;
use crate::connectors::continue_dev::ContinueConnector;
use crate::connectors::{Connector, Origin};
use franken_agent_detection::connectors::SourceScanHooks;

fn seed(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        path,
        serde_json::to_vec(&serde_json::json!({
            "sessionId": path.file_stem().unwrap().to_str().unwrap(),
            "title": "strict discovery fixture",
            "workspaceDirectory": "/remote/project",
            "history": [{"message": {"role": "user", "content": "retained question"}}]
        }))
        .unwrap(),
    )
    .unwrap();
}

fn context(root: ScanRoot, data: &Path) -> ScanContext {
    ScanContext::with_roots(data.to_path_buf(), vec![root], None)
}

fn exercise(ctx: &ScanContext) -> Vec<DiscoveredSourceFile> {
    let connector = ContinueConnector::new();
    let expected = connector.discover_allowed(ctx, &|_| false).unwrap();
    assert!(!expected.is_empty(), "the fixture must contain real sessions");
    let mut admitted = Vec::new();
    let mut completed = Vec::new();
    let mut delivered = 0;
    connector
        .scan_allowed(
            ctx,
            &mut SourceScanHooks {
                should_scan_source: Some(&mut |source| {
                    admitted.push(source.clone());
                    true
                }),
                on_source_complete: Some(&mut |completion| {
                    assert_eq!(completion.conversations_emitted, 1);
                    assert!(completion.required_sidecars.is_empty());
                    completed.push(completion.source.clone());
                    Ok(())
                }),
            },
            &mut |conversation| {
                assert_eq!(conversation.agent_slug, "continue");
                assert_eq!(conversation.messages[0].content, "retained question");
                delivered += 1;
                Ok(())
            },
            &|_| false,
        )
        .unwrap();
    assert_eq!(delivered, expected.len());
    assert_eq!(admitted, expected);
    assert_eq!(completed, expected);
    expected
}

#[test]
fn ordinary_layouts_keep_the_pinned_discovery_identity_and_checkpoint() {
    let temp = tempfile::tempdir().unwrap();
    let profile = temp.path().join("profile");
    let file = profile.join(".continue/sessions/native.json");
    seed(&file);
    seed(&profile.join("sessions/relocated.json"));
    fs::write(profile.join(".continue/sessions/sessions.json"), b"not a transcript").unwrap();
    fs::write(profile.join(".continue/sessions/notes.txt"), b"not a transcript").unwrap();
    seed(&profile.join(".continue/sessions/child/ignored.json"));
    for root in [
        profile.clone(),
        profile.join(".continue"),
        profile.join(".continue/sessions"),
        file,
    ] {
        let ctx = context(ScanRoot::local(root), &temp.path().join("cass"));
        let native = franken_agent_detection::connectors::continue_dev::ContinueConnector::new()
            .discover_source_files(&ctx)
            .unwrap();
        let actual = exercise(&ctx);
        assert_eq!(actual, native, "do not migrate existing ledger identities");
    }
}

#[test]
fn basename_preserving_mirrors_use_remote_store_identity_and_keep_provenance() {
    for (remote, relative) in [
        ("~/.continue", ".continue/sessions"),
        ("/home/user/.continue/sessions", "sessions"),
        ("C:\\Users\\user\\.continue\\sessions", "sessions"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let container = temp.path().join("opaque-container");
        seed(&container.join(relative).join("remote.json"));
        // A payload outside the declared store must not be parsed.
        seed(&container.join("unrelated.json"));
        let root = ScanRoot::remote(container.clone(), Origin::remote("workstation"), None)
            .with_rewrite("/remote", "/local")
            .with_mirrored_source_path(remote);
        let ctx = context(root, &temp.path().join("cass"));
        let sources = exercise(&ctx);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].origin, Origin::remote("workstation"));
        assert_eq!(sources[0].scan_root, container.join(relative));
        let mut rows = Vec::new();
        ContinueConnector::new().scan_allowed(
            &ctx, &mut SourceScanHooks::default(),
            &mut |row| { rows.push(row); Ok(()) }, &|_| false,
        ).unwrap();
        assert_eq!(rows[0].workspace, Some(PathBuf::from("/local/project")));
        assert_eq!(
            rows[0].metadata["origin"],
            serde_json::to_value(Origin::remote("workstation")).unwrap()
        );
        assert_eq!(ctx.scan_roots[0].path, container, "leave the caller's selector intact");
        assert_eq!(ctx.scan_roots[0].mirrored_source_path.as_deref(), Some(remote));
    }
}

#[test]
fn contents_only_mirrors_do_not_require_a_second_basename_directory() {
    for (remote, relative) in [
        ("~/.continue/", "sessions"),
        ("~/.continue/sessions/", ""),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let container = temp.path().join("contents");
        seed(&container.join(relative).join("session.json"));
        let ctx = context(
            ScanRoot::remote(container.clone(), Origin::remote("workstation"), None)
                .with_mirrored_source_path(remote),
            &temp.path().join("cass"),
        );
        let sources = exercise(&ctx);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].scan_root, container.join(relative));
    }
}

#[test]
fn unrelated_mirrors_never_reach_the_continue_parser_or_implicit_home() {
    let temp = tempfile::tempdir().unwrap();
    let container = temp.path().join("mentions-.continue-but-is-not-owned");
    seed(&container.join("sessions/codex.json"));
    let data = temp.path().join(".continue");
    seed(&data.join("sessions/default.json"));
    for remote in ["~/.codex/sessions", "~/sessions", "~/.continue-copy/sessions"] {
        let ctx = context(
            ScanRoot::remote(container.clone(), Origin::remote("other"), None)
                .with_mirrored_source_path(remote),
            &data,
        );
        let connector = ContinueConnector::new();
        assert!(connector.discover_allowed(&ctx, &|_| false).unwrap().is_empty());
        connector.scan_allowed(
            &ctx,
            &mut SourceScanHooks {
                should_scan_source: Some(&mut |_| panic!("unowned source admitted")),
                on_source_complete: Some(&mut |_| panic!("unowned source completed")),
            },
            &mut |_| panic!("unowned mirror or implicit home parsed"),
            &|_| false,
        ).unwrap();
    }
}

#[test]
fn excluded_derived_stores_do_not_fall_back_to_parent_json_files() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("profile");
    let sessions = root.join(".continue/sessions");
    seed(&sessions.join("private.json"));
    seed(&root.join("parent.json"));
    let ctx = context(ScanRoot::local(root), &temp.path().join("cass"));
    let excluded = |path: &Path| path.starts_with(&sessions);
    let connector = ContinueConnector::new();
    assert!(connector.discover_allowed(&ctx, &excluded).unwrap().is_empty());
    connector.scan_allowed(
        &ctx, &mut SourceScanHooks::default(),
        &mut |_| panic!("excluded store broadened to its parent"), &excluded,
    ).unwrap();
}

#[test]
fn missing_explicit_roots_fail_without_scanning_the_default_home() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join(".continue");
    seed(&data.join("sessions/default.json"));
    let ctx = context(ScanRoot::local(temp.path().join("missing")), &data);
    let connector = ContinueConnector::new();
    let error = connector.discover_allowed(&ctx, &|_| false).unwrap_err();
    assert_eq!(error.downcast_ref::<io::Error>().unwrap().kind(), io::ErrorKind::NotFound);
    assert!(connector.scan_allowed(
        &ctx, &mut SourceScanHooks::default(),
        &mut |_| panic!("missing selection broadened to home"), &|_| false,
    ).is_err());
}

#[test]
fn moved_inventory_members_cannot_disappear_between_discovery_and_parsing() {
    let temp = tempfile::tempdir().unwrap();
    let first = temp.path().join("a.json");
    let second = temp.path().join("b.json");
    seed(&first);
    seed(&second);
    let ctx = context(ScanRoot::local(temp.path().to_path_buf()), &temp.path().join("cass"));
    let mut delivered = 0;
    let mut completed = 0;
    let error = ContinueConnector::new().scan_allowed(
        &ctx,
        &mut SourceScanHooks {
            should_scan_source: None,
            on_source_complete: Some(&mut |_| { completed += 1; Ok(()) }),
        },
        &mut |_| {
            delivered += 1;
            fs::rename(&second, temp.path().join("b.retained")).unwrap();
            Ok(())
        },
        &|_| false,
    ).unwrap_err();
    assert_eq!((delivered, completed), (1, 1));
    assert!(error.to_string().contains("1 admitted snapshot(s) did not complete"));
    assert!(error.to_string().contains("b.json"));
    assert!(temp.path().join("b.retained").is_file());
}

#[test]
fn changed_observations_are_refused_before_delivering_replacement_contents() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("session.json");
    seed(&file);
    let ctx = context(ScanRoot::local(temp.path().to_path_buf()), &temp.path().join("cass"));
    let error = ContinueConnector::new().scan_allowed(
        &ctx,
        &mut SourceScanHooks {
            should_scan_source: Some(&mut |_| {
                // Change size deterministically even on coarse-mtime filesystems.
                use std::io::Write;
                fs::OpenOptions::new().append(true).open(&file).unwrap()
                    .write_all(b"\n ").unwrap();
                true
            }),
            on_source_complete: Some(&mut |_| panic!("changed observation certified")),
        },
        &mut |_| panic!("replacement contents crossed old admission"),
        &|_| false,
    ).unwrap_err();
    assert!(error.to_string().contains("1 admitted snapshot(s) did not complete"));
}

#[cfg(unix)]
#[test]
fn filesystem_discovery_errors_are_not_empty_successes_and_exclusions_still_apply() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let bad = temp.path().join("private.json");
    symlink("private.json", &bad).unwrap();
    seed(&temp.path().join("public.json"));
    let ctx = context(ScanRoot::local(temp.path().to_path_buf()), &temp.path().join("cass"));
    let connector = ContinueConnector::new();
    let error = connector.discover_allowed(&ctx, &|_| false).unwrap_err();
    assert!(error.downcast_ref::<io::Error>().is_some());
    assert!(error.to_string().contains("private.json"));
    let excluded = |path: &Path| path.file_name().is_some_and(|name| name == "private.json");
    let sources = connector.discover_allowed(&ctx, &excluded).unwrap();
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].source_path.file_name().unwrap(), "public.json");
    connector.scan_allowed(
        &ctx, &mut SourceScanHooks::default(), &mut |_| Ok(()), &excluded,
    ).unwrap();
}

#[cfg(unix)]
#[test]
fn inferred_mirror_stores_never_follow_a_symlinked_transport_boundary() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let real = temp.path().join("real");
    seed(&real.join("sessions/session.json"));
    let alias = temp.path().join("alias");
    symlink(&real, &alias).unwrap();
    let ctx = context(
        ScanRoot::remote(alias, Origin::remote("workstation"), None)
            .with_mirrored_source_path("~/.continue/sessions"),
        &temp.path().join("cass"),
    );
    assert!(ContinueConnector::new().discover_allowed(&ctx, &|_| false).is_err());
}

#[test]
fn an_absent_default_installation_is_empty_not_an_explicit_root_failure() {
    let temp = tempfile::tempdir().unwrap();
    let ctx = ScanContext::local_default(temp.path().join(".continue"), None);
    let connector = ContinueConnector::new();
    assert!(connector.discover_allowed(&ctx, &|_| false).unwrap().is_empty());
    connector
        .scan_allowed(
            &ctx,
            &mut SourceScanHooks::default(),
            &mut |_| panic!("absent default installation emitted a session"),
            &|_| false,
        )
        .unwrap();
}

#[test]
fn metadata_discovery_pulses_liveness_without_parsing_snapshot_contents() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let temp = tempfile::tempdir().unwrap();
    for index in 0..128 {
        fs::write(temp.path().join(format!("{index:03}.json")), b"not JSON").unwrap();
    }
    let pulses = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&pulses);
    let ctx = context(
        ScanRoot::local(temp.path().to_path_buf()),
        &temp.path().join("cass"),
    )
    .with_progress_tick(Arc::new(move || {
        observed.fetch_add(1, Ordering::Relaxed);
    }));
    let sources = ContinueConnector::new()
        .discover_allowed(&ctx, &|_| false)
        .unwrap();
    assert_eq!(sources.len(), 128);
    assert!(pulses.load(Ordering::Relaxed) >= 3);
}
