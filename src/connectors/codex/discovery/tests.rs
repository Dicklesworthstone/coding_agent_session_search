use super::*;
use crate::connectors::codex::CodexConnector;
use franken_agent_detection::{
    CodexConnector as FadConnector, Connector, Origin, Platform, SourceCompletion, SourceScanHooks,
};
#[cfg(unix)]
use std::process::Command;
use std::time::{Duration, UNIX_EPOCH};

fn message(text: &str) -> String {
    format!(
        "{}\n",
        serde_json::json!({"type":"response_item","payload":{"role":"user","content":text}})
    )
}

fn seed(path: &Path, text: &str) -> Result<()> {
    fs::create_dir_all(path.parent().unwrap())?;
    let content = if path
        .extension()
        .is_some_and(|extension| extension == "json")
    {
        serde_json::json!({"items":[{"role":"user","content":text}]}).to_string()
    } else {
        message(text)
    };
    if is_compressed_rollout(path) {
        fs::write(path, zstd::stream::encode_all(content.as_bytes(), 0)?)?;
    } else {
        fs::write(path, content)?;
    }
    Ok(())
}

fn assert_fad_inventory(ctx: &ScanContext) -> Result<()> {
    let expected = FadConnector::new().discover_source_files(ctx)?;
    let mut inventory = Inventory::default();
    inventory.collect(ctx, &ScanExclusions::from_env());
    assert_eq!(inventory.finish()?, expected, "{:#?}", ctx.scan_roots);
    Ok(())
}

#[test]
fn healthy_inventory_matches_pinned_fad_selection_order_cutoff_and_provenance() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let home = temp.path().join(".codex");
    let sessions = home.join("sessions");
    let day = sessions.join("2026/10/19");
    let names = [
        "rollout-a.jsonl",
        "rollout-b.JSONL",
        "rollout-c.json",
        "rollout-d.jsonl.zst",
        "rollout-e.JSONL.ZST",
        "rollout-pair.jsonl",
        "rollout-pair.jsonl.zst",
        "nested/rollout-a.jsonl",
    ];
    for (index, name) in names.iter().enumerate() {
        let path = day.join(name);
        seed(&path, name)?;
        fs::OpenOptions::new()
            .write(true)
            .open(&path)?
            .set_modified(UNIX_EPOCH + Duration::from_millis(20_000_000 + index as u64 * 5_000))?;
    }
    fs::write(day.join("not-a-rollout.jsonl"), "ignored")?;
    let shapes = [
        vec![home.clone()],
        vec![sessions.clone()],
        vec![day.clone()],
        vec![temp.path().to_path_buf()],
        vec![day.join("rollout-pair.jsonl.zst")],
        vec![home.clone(), sessions, day.join("rollout-a.jsonl")],
    ];
    for shape in shapes {
        for since in [None, Some(0), Some(20_031_000), Some(i64::MAX)] {
            let roots = shape
                .iter()
                .map(|path| {
                    ScanRoot::remote(
                        path.clone(),
                        Origin::remote("workstation"),
                        Some(Platform::Linux),
                    )
                    .with_rewrite("/remote/project", "/local/project")
                })
                .collect();
            let ctx = ScanContext::with_roots(temp.path().join("cass"), roots, since);
            assert_fad_inventory(&ctx)?;
        }
    }
    // FAD's data-dir override is independent of CODEX_HOME/default detection.
    assert_fad_inventory(&ScanContext::local_default(home, None))?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn nested_symlinks_do_not_widen_directory_selection() -> Result<()> {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir()?;
    let sessions = temp.path().join(".codex/sessions");
    let local = sessions.join("rollout-local.jsonl");
    seed(&local, "local")?;
    let foreign = temp.path().join("foreign/rollout-foreign.jsonl");
    seed(&foreign, "outside selected directory")?;
    let linked_file = sessions.join("rollout-linked.jsonl");
    symlink(&foreign, &linked_file)?;
    symlink(foreign.parent().unwrap(), sessions.join("linked-directory"))?;
    for selected in [sessions.clone(), linked_file] {
        let ctx = ScanContext::with_roots(
            temp.path().join("cass"),
            vec![ScanRoot::local(selected)],
            None,
        );
        assert_fad_inventory(&ctx)?;
    }
    let ctx = ScanContext::with_roots(
        temp.path().join("cass"),
        vec![ScanRoot::local(sessions)],
        None,
    );
    let discovered = CodexConnector::new().discover_source_files(&ctx)?;
    assert_eq!(discovered.len(), 1);
    assert_eq!(discovered[0].source_path, local);
    Ok(())
}

const CHILD_ROOT: &str = "CASS_CODEX_DISCOVERY_TEST_ROOT";
const CHILD_MODE: &str = "CASS_CODEX_DISCOVERY_TEST_MODE";

#[cfg(unix)]
#[test]
fn unreadable_active_and_archive_directories_preserve_neighbors_and_retry() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let home = temp.path().join(".codex");
    for collection in ["sessions", "archived_sessions"] {
        for directory in ["a", "b", "c"] {
            seed(
                &home
                    .join(collection)
                    .join(directory)
                    .join("rollout-test.jsonl"),
                &format!("{collection}-{directory}"),
            )?;
        }
    }
    let denied = [home.join("sessions/b"), home.join("archived_sessions/b")];
    run_permission_cases(
        temp.path(),
        &home,
        &denied,
        [
            ("unreadable", String::new()),
            (
                "excluded",
                format!("{},{}", denied[0].display(), denied[1].display()),
            ),
            ("repaired", String::new()),
        ],
    )
}

#[cfg(unix)]
#[test]
fn policy_resolution_failure_preserves_neighbors_for_explicit_roots_and_files() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let home = temp.path().join(".codex");
    for directory in ["a", "blocked", "c"] {
        seed(
            &home.join(directory).join("sessions/rollout-test.jsonl"),
            directory,
        )?;
    }
    // The walk never follows nested symlinks, even rollout-named ones whose
    // namespace cannot resolve. Policy errors must not widen that selection.
    std::os::unix::fs::symlink(
        "rollout-loop.jsonl",
        home.join("a/sessions/rollout-loop.jsonl"),
    )?;
    let unrelated = temp.path().join("unrelated");
    fs::create_dir(&unrelated)?;
    let policy = unrelated.to_string_lossy().into_owned();
    let denied = home.join("blocked");
    run_permission_cases(
        temp.path(),
        &home,
        std::slice::from_ref(&denied),
        [
            ("namespace-unreadable", policy.clone()),
            ("namespace-excluded", denied.to_string_lossy().into_owned()),
            ("namespace-repaired", policy),
        ],
    )
}

#[cfg(unix)]
fn run_permission_cases(
    temp: &Path,
    home: &Path,
    denied: &[PathBuf],
    cases: [(&str, String); 3],
) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::os::unix::process::CommandExt;

    // Keep one fixture's fork from inheriting another fixture's executable
    // copy writer, which can make that copy fail to execute with ETXTBSY.
    static PERMISSION_CASES: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = PERMISSION_CASES
        .lock()
        .map_err(|_| anyhow::anyhow!("Codex permission fixture lock poisoned"))?;

    fs::set_permissions(temp, fs::Permissions::from_mode(0o755))?;
    for path in denied {
        fs::set_permissions(path, fs::Permissions::from_mode(0o000))?;
    }
    // Some single-ID test namespaces report uid 0 but have no DAC-bypass
    // capabilities and cannot switch to another uid. Keep their effective
    // permission denial; use an unprivileged uid when root can bypass it.
    let drop_root_privileges =
        fs::metadata(temp)?.uid() == 0 && denied.iter().any(|path| fs::read_dir(path).is_ok());
    let executable = if drop_root_privileges {
        // A test binary may live below a root-only Cargo directory. Copy it
        // into this accessible fixture only when the child drops privileges.
        let executable = temp.join("discovery-test-child");
        fs::copy(std::env::current_exe()?, &executable)?;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))?;
        executable
    } else {
        std::env::current_exe()?
    };
    let run = |(mode, policy): (&str, String)| -> Result<std::process::Output> {
        let mut command = Command::new(&executable);
        command
            .args(["codex::discovery::tests::permission_child", "--nocapture"])
            .current_dir(temp)
            .env(CHILD_ROOT, home)
            .env(CHILD_MODE, mode)
            .env_remove("CASS_CODEX_MAX_SOURCE_BYTES")
            .env("CASS_EXCLUDE_PATHS", policy);
        if drop_root_privileges {
            command.gid(65534).uid(65534);
        }
        Ok(command.output()?)
    };
    let [unreadable, excluded, repaired] = cases;
    let unreadable = run(unreadable);
    let excluded = run(excluded);
    // Restore access before asserting, including when the child failed, so
    // the fixture can be reclaimed normally by TempDir.
    for path in denied {
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    }
    let repaired = run(repaired);
    for result in [unreadable, excluded, repaired] {
        let output = result?;
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout)
                .contains("test result: ok. 1 passed; 0 failed;"),
            "the permission-child selector must run exactly one successful test: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
    Ok(())
}

#[test]
fn permission_child() -> Result<()> {
    let Ok(home) = dotenvy::var(CHILD_ROOT) else {
        return Ok(());
    };
    let home = PathBuf::from(home);
    let mode = dotenvy::var(CHILD_MODE)?;
    if mode.starts_with("namespace-") {
        return policy_namespace_child(&home, &mode);
    }
    let ctx = ScanContext::with_roots(home.join("cass"), vec![ScanRoot::local(home.clone())], None);
    let mut expected = Vec::new();
    for collection in ["sessions", "archived_sessions"] {
        for directory in ["a", "b", "c"] {
            if directory != "b" || mode == "repaired" {
                expected.push(
                    home.join(collection)
                        .join(directory)
                        .join("rollout-test.jsonl"),
                );
            }
        }
    }
    let connector = CodexConnector::new();
    let mut completed = Vec::new();
    let mut complete = |done: &SourceCompletion| {
        completed.push(done.source.source_path.clone());
        Ok(())
    };
    let mut hooks = SourceScanHooks {
        should_scan_source: None,
        on_source_complete: Some(&mut complete),
    };
    let mut delivered = Vec::new();
    let result = connector.scan_with_source_boundaries(&ctx, &mut hooks, &mut |conversation| {
        assert_eq!(conversation.messages.len(), 1);
        delivered.push(conversation.source_path);
        Ok(())
    });
    assert_eq!(delivered, expected);
    assert_eq!(completed, expected);
    if mode == "unreadable" {
        // Establish actual EACCES, not root's permission-insensitive behavior.
        assert_eq!(
            fs::read_dir(home.join("sessions/b")).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        // The incumbent live in this invocation exhibits the original loss:
        // its inventory succeeds while omitting the inaccessible active file.
        let incumbent = FadConnector::new().discover_source_files(&ctx)?;
        assert_eq!(incumbent.len(), 2);
        let error = result.unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::PermissionDenied
        );
        let report = error.downcast_ref::<FailureReport>().unwrap();
        assert_eq!(report.failed_path_count, 2);
        assert_eq!(report.omitted_path_count, 0);
        assert_eq!(report.failed_paths.len(), 2);
        assert!(
            report
                .failed_paths
                .iter()
                .any(|failure| failure.source_path == home.join("sessions/b").to_string_lossy())
        );
        assert!(report.failed_paths.iter().any(
            |failure| failure.source_path == home.join("archived_sessions/b").to_string_lossy()
        ));
        assert_eq!(
            error.chain().find_map(|cause| cause
                .downcast_ref::<walkdir::Error>()
                .and_then(walkdir::Error::path)),
            Some(home.join("sessions/b").as_path())
        );
        assert!(connector.discover_source_files(&ctx).is_err());
        assert!(connector.scan(&ctx).is_err());
    } else {
        result?;
        assert_eq!(connector.discover_source_files(&ctx)?.len(), expected.len());
        assert_eq!(connector.scan(&ctx)?.len(), expected.len());
    }
    Ok(())
}

fn policy_namespace_child(home: &Path, mode: &str) -> Result<()> {
    let blocked_sessions = home.join("blocked/sessions");
    let blocked_file = blocked_sessions.join("rollout-test.jsonl");
    let mut expected = vec![home.join("a/sessions/rollout-test.jsonl")];
    if mode == "namespace-repaired" {
        expected.push(blocked_file.clone());
    }
    expected.push(home.join("c/sessions/rollout-test.jsonl"));
    for selected in [blocked_sessions, blocked_file] {
        if mode != "namespace-repaired" {
            // The selected path itself cannot be resolved through this
            // ancestor; a mode-000 selected directory alone does not prove it.
            assert_eq!(
                fs::metadata(&selected).unwrap_err().kind(),
                io::ErrorKind::PermissionDenied
            );
            assert_eq!(
                fs::canonicalize(&selected).unwrap_err().kind(),
                io::ErrorKind::PermissionDenied
            );
        }
        let ctx = ScanContext::with_roots(
            home.join("cass"),
            vec![
                ScanRoot::local(home.join("a")),
                ScanRoot::local(selected.clone()),
                ScanRoot::local(home.join("c")),
            ],
            None,
        );
        let connector = CodexConnector::new();
        let mut completed = Vec::new();
        let mut complete = |done: &SourceCompletion| {
            completed.push(done.source.source_path.clone());
            Ok(())
        };
        let mut hooks = SourceScanHooks {
            should_scan_source: None,
            on_source_complete: Some(&mut complete),
        };
        let mut delivered = Vec::new();
        let result = connector.scan_with_source_boundaries(&ctx, &mut hooks, &mut |conversation| {
            assert_eq!(conversation.messages.len(), 1);
            delivered.push(conversation.source_path);
            Ok(())
        });
        assert_eq!(delivered, expected);
        assert_eq!(completed, expected);
        if mode == "namespace-unreadable" {
            // The incumbent in this invocation still silently omits the
            // inaccessible selection under an unrelated, valid exclusion.
            let incumbent: Vec<_> = FadConnector::new()
                .discover_source_files(&ctx)?
                .into_iter()
                .map(|source| source.source_path)
                .collect();
            assert_eq!(incumbent, expected);
            for error in [
                result.unwrap_err(),
                connector.discover_source_files(&ctx).unwrap_err(),
                connector.scan(&ctx).unwrap_err(),
            ] {
                assert_eq!(
                    error.downcast_ref::<io::Error>().unwrap().kind(),
                    io::ErrorKind::PermissionDenied
                );
                let report = error.downcast_ref::<FailureReport>().unwrap();
                assert_eq!(report.reason, "source_discovery_failures");
                assert_eq!(report.failed_path_count, 1);
                assert_eq!(report.omitted_path_count, 0);
                assert_eq!(report.failed_paths.len(), 1);
                let failure = report.failed_paths.first().unwrap();
                assert_eq!(failure.source_path, selected.to_string_lossy());
                assert_eq!(failure.reason, "source_unreadable");
            }
        } else {
            result?;
            let discovered: Vec<_> = connector
                .discover_source_files(&ctx)?
                .into_iter()
                .map(|source| source.source_path)
                .collect();
            let scanned: Vec<_> = connector
                .scan(&ctx)?
                .into_iter()
                .map(|conversation| conversation.source_path)
                .collect();
            assert_eq!(discovered, expected);
            assert_eq!(scanned, expected);
        }
    }
    Ok(())
}

#[test]
fn directory_failure_bookkeeping_and_diagnostics_are_bounded() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut inventory = Inventory::default();
    for index in 0..MAX_FAILURE_SAMPLES + 8 {
        let path = temp.path().join(format!("missing-{index}"));
        let error = fs::read_dir(&path).unwrap_err();
        inventory.record_failure(&path, error.into());
    }
    assert_eq!(inventory.failed_paths.len(), MAX_FAILURE_SAMPLES);
    assert_eq!(inventory.report.failed_paths.len(), MAX_FAILURE_SAMPLES);
    assert_eq!(inventory.report.failed_path_count, MAX_FAILURE_SAMPLES + 8);
    assert_eq!(inventory.report.omitted_path_count, 8);
    let retained = temp.path().join("missing-0");
    inventory.record_failure(&retained, fs::read_dir(&retained).unwrap_err().into());
    assert_eq!(inventory.report.failed_path_count, MAX_FAILURE_SAMPLES + 8);
    let omitted = temp.path().join(format!("missing-{MAX_FAILURE_SAMPLES}"));
    inventory.record_failure(&omitted, fs::read_dir(&omitted).unwrap_err().into());
    assert_eq!(inventory.failed_paths.len(), MAX_FAILURE_SAMPLES);
    assert_eq!(inventory.report.omitted_path_count, 9);
    assert_eq!(inventory.report.failed_path_count, MAX_FAILURE_SAMPLES + 9);
    Ok(())
}
