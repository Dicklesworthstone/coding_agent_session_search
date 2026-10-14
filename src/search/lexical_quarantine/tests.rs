use super::*;

fn bundle(root: &Path, name: &str) -> PathBuf {
    let path = root.join(format!("{QUARANTINE_PREFIX}{name}"));
    fs::create_dir_all(path.join("index")).unwrap();
    fs::write(path.join("index/MANIFEST"), b"derived index evidence").unwrap();
    path
}

fn old_now() -> SystemTime {
    SystemTime::now() + Duration::from_secs(24 * 60 * 60)
}

fn scan(path: &Path) -> Inventory {
    inspect_at(path, old_now(), true, Limits::default()).unwrap()
}

#[test]
fn global_retention_keeps_one_newest_bundle_across_both_historical_roots() {
    let data = tempfile::tempdir().unwrap();
    let first = bundle(data.path(), "000001");
    let second = bundle(&data.path().join("index"), "000002");
    let report = scan(data.path());
    assert!(report.complete);
    assert_eq!(report.artifacts.len(), 2);
    assert_eq!(
        report
            .artifacts
            .iter()
            .filter(|a| a.disposition == Disposition::RetainNewest)
            .count(),
        1
    );
    assert_eq!(
        report
            .artifacts
            .iter()
            .filter(|a| a.disposition == Disposition::Eligible)
            .count(),
        1
    );
    assert_eq!(
        report.total_bytes,
        2 * b"derived index evidence".len() as u64
    );
    assert_eq!(
        report.eligible_bytes,
        b"derived index evidence".len() as u64
    );
    assert_eq!(
        fs::read(first.join("index/MANIFEST")).unwrap(),
        b"derived index evidence"
    );
    assert_eq!(
        fs::read(second.join("index/MANIFEST")).unwrap(),
        b"derived index evidence"
    );
}

#[test]
fn inspection_does_not_open_database_payloads_or_modify_evidence() {
    let data = tempfile::tempdir().unwrap();
    fs::write(data.path().join("agent_search.db"), b"not a database").unwrap();
    let path = bundle(&data.path().join("index"), "000001");
    let original = scan(data.path());
    let again = scan(data.path());
    assert_eq!(
        original.artifacts[0].metadata_fingerprint,
        again.artifacts[0].metadata_fingerprint
    );
    assert_eq!(
        fs::read(path.join("index/MANIFEST")).unwrap(),
        b"derived index evidence"
    );
    assert_eq!(
        fs::read(data.path().join("agent_search.db")).unwrap(),
        b"not a database"
    );
    assert!(!data.path().join("index-run.lock").exists());
}

#[test]
fn checkpoint_only_evidence_is_listed_but_unknown_layout_refuses_reclamation() {
    let data = tempfile::tempdir().unwrap();
    let path = data.path().join(format!("{QUARANTINE_PREFIX}000001"));
    fs::create_dir(&path).unwrap();
    fs::write(
        path.join(CHECKPOINT),
        b"even an unreadable checkpoint is evidence",
    )
    .unwrap();
    let report = scan(data.path());
    assert!(report.complete);
    assert_eq!(report.artifacts[0].disposition, Disposition::RetainNewest);
    bundle(data.path(), "000002");
    fs::write(path.join("operator-notes"), b"do not delete").unwrap();
    let report = scan(data.path());
    assert!(!report.complete);
    assert_eq!(report.eligible_bytes, 0);
    assert!(
        report
            .artifacts
            .iter()
            .all(|a| a.disposition == Disposition::Inspect)
    );
}

#[test]
fn recent_and_future_changes_anywhere_in_bundle_prevent_reclamation() {
    let data = tempfile::tempdir().unwrap();
    let first = bundle(data.path(), "000001");
    bundle(data.path(), "000002");
    let now = SystemTime::now();
    let report = inspect_at(data.path(), now, true, Limits::default()).unwrap();
    assert_eq!(report.eligible_bytes, 0);
    fs::File::open(first.join("index/MANIFEST"))
        .unwrap()
        .set_modified(now + Duration::from_secs(48 * 60 * 60))
        .unwrap();
    let report = scan(data.path());
    let first = report.artifacts.iter().find(|a| a.path == first).unwrap();
    assert_eq!(first.age_seconds, None);
    assert_eq!(first.disposition, Disposition::RetainNewest);
}

#[test]
fn bare_prefix_similar_names_live_indexes_and_publish_backups_are_not_candidates() {
    let data = tempfile::tempdir().unwrap();
    for name in [
        QUARANTINE_PREFIX,
        ".lexical-rebuild-quarantine-too-long",
        "v9-quill",
        ".lexical-publish-backups",
        "cass-lexical-shards.123456",
        "source-project",
    ] {
        fs::create_dir(data.path().join(name)).unwrap();
    }
    assert!(scan(data.path()).artifacts.is_empty());
    assert!(valid_name(
        ".lexical-rebuild-quarantine-Ab0cD9",
        QUARANTINE_PREFIX
    ));
    assert!(!valid_name(
        ".lexical-rebuild-quarantine-../foo",
        QUARANTINE_PREFIX
    ));
}

#[test]
fn incomplete_work_budgets_cannot_authorize_a_partial_inventory() {
    let data = tempfile::tempdir().unwrap();
    bundle(data.path(), "000001");
    bundle(data.path(), "000002");
    for limits in [
        Limits {
            root_entries: 0,
            ..Default::default()
        },
        Limits {
            bundles: 1,
            ..Default::default()
        },
        Limits {
            tree_entries: 1,
            ..Default::default()
        },
        Limits {
            path_bytes: 1,
            ..Default::default()
        },
        Limits {
            depth: 1,
            ..Default::default()
        },
        Limits {
            timeout: Duration::ZERO,
            ..Default::default()
        },
    ] {
        let report = inspect_at(data.path(), old_now(), true, limits).unwrap();
        assert!(!report.complete);
        assert_eq!(report.eligible_bytes, 0);
        assert!(
            report
                .artifacts
                .iter()
                .all(|a| a.disposition != Disposition::Eligible)
        );
    }
    assert_eq!(scan(data.path()).artifacts.len(), 2);
}

#[test]
fn disabled_retention_and_missing_data_directory_are_explicit() {
    let data = tempfile::tempdir().unwrap();
    bundle(data.path(), "000001");
    bundle(data.path(), "000002");
    let report = inspect_at(data.path(), old_now(), false, Limits::default()).unwrap();
    assert!(report.complete);
    assert!(
        report
            .artifacts
            .iter()
            .all(|a| a.disposition == Disposition::RetainDisabled)
    );
    assert_eq!(report.eligible_bytes, 0);
    let missing = data.path().join("missing");
    assert_eq!(
        inspect_at(&missing, old_now(), true, Limits::default())
            .unwrap_err()
            .kind(),
        io::ErrorKind::NotFound
    );
    assert!(!missing.exists());
}

#[test]
fn interrupted_reclaim_is_recognized_without_displacing_the_newest_evidence() {
    let data = tempfile::tempdir().unwrap();
    bundle(data.path(), "000001");
    let pending = data.path().join(format!("{RECLAIM_PREFIX}000002"));
    fs::create_dir_all(pending.join("quarantine/index")).unwrap();
    fs::write(pending.join("quarantine/index/MANIFEST"), b"interrupted").unwrap();
    let report = scan(data.path());
    assert!(report.complete);
    assert_eq!(
        report
            .artifacts
            .iter()
            .filter(|a| a.disposition == Disposition::RetainNewest)
            .count(),
        1
    );
    assert_eq!(
        report
            .artifacts
            .iter()
            .find(|a| a.interrupted_reclaim)
            .unwrap()
            .disposition,
        Disposition::Eligible
    );
}

#[cfg(unix)]
#[test]
fn symlinked_roots_bundles_and_nested_files_are_never_followed() {
    use std::os::unix::fs::symlink;
    let data = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret"), b"outside").unwrap();
    symlink(outside.path(), data.path().join("index")).unwrap();
    assert!(inspect_at(data.path(), old_now(), true, Limits::default()).is_err());
    let other = tempfile::tempdir().unwrap();
    symlink(
        outside.path(),
        other.path().join(format!("{QUARANTINE_PREFIX}000001")),
    )
    .unwrap();
    let path = bundle(other.path(), "000002");
    symlink(outside.path(), path.join("index/external")).unwrap();
    let report = scan(other.path());
    assert!(!report.complete);
    assert_eq!(report.eligible_bytes, 0);
    assert_eq!(fs::read(outside.path().join("secret")).unwrap(), b"outside");
}

#[cfg(unix)]
#[test]
fn hard_linked_payloads_require_inspection_instead_of_double_counted_reclaim() {
    let data = tempfile::tempdir().unwrap();
    let path = bundle(data.path(), "000001");
    fs::hard_link(
        path.join("index/MANIFEST"),
        data.path().join("retained-link"),
    )
    .unwrap();
    let report = scan(data.path());
    assert!(!report.complete);
    assert_eq!(report.eligible_bytes, 0);
    assert_eq!(report.artifacts[0].disposition, Disposition::Inspect);
}
