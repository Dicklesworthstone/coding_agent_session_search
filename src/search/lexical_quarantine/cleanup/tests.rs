use super::*;
use std::fs::{File, OpenOptions};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn bundle(root: &Path, suffix: &str) -> PathBuf {
    let path = root.join(format!("{QUARANTINE_PREFIX}{suffix}"));
    fs::create_dir_all(path.join("index")).unwrap();
    fs::write(path.join("index/MANIFEST"), b"derived evidence").unwrap();
    path
}

fn future() -> SystemTime {
    SystemTime::now() + Duration::from_secs(24 * 60 * 60)
}

fn populated(root: &Path) {
    bundle(root, "000001");
    bundle(&root.join("index"), "000002");
    bundle(&root.join("index"), "000003");
}

fn preview(root: &Path, now: SystemTime) -> SealedPlan {
    seal(root, now, true, Limits::default()).unwrap()
}

fn surviving_paths(plan: &CleanupPlan) -> Vec<PathBuf> {
    plan.inventory
        .artifacts
        .iter()
        .filter(|a| a.disposition != Disposition::Eligible)
        .map(|a| a.path.clone())
        .collect()
}

#[test]
fn preview_is_read_only_and_approval_survives_clock_ticks_and_lock_creation() -> TestResult {
    let root = tempfile::tempdir()?;
    populated(root.path());
    let instant = future();
    let before = preview(root.path(), instant);
    assert!(valid_approval(
        before.view.approval_fingerprint.as_deref().unwrap()
    ));
    assert!(!root.path().join("index-run.lock").exists());
    let next = preview(root.path(), instant + Duration::from_secs(1));
    assert_eq!(
        before.view.approval_fingerprint,
        next.view.approval_fingerprint
    );
    let _lease = lease::Lease::acquire(root.path())?;
    let after_lock = preview(root.path(), instant + Duration::from_secs(2));
    assert_eq!(
        before.view.approval_fingerprint,
        after_lock.view.approval_fingerprint
    );
    assert_eq!(after_lock.view.inventory.artifacts.len(), 3);
    Ok(())
}

#[test]
fn approved_cleanup_preserves_newest_archive_live_index_and_lock_metadata() -> TestResult {
    let root = tempfile::tempdir()?;
    populated(root.path());
    fs::write(
        root.path().join("agent_search.db"),
        b"unreadable archive sentinel",
    )?;
    fs::create_dir_all(root.path().join("index/v9-quill"))?;
    fs::write(root.path().join("index/v9-quill/MANIFEST"), b"live")?;
    fs::write(root.path().join("index-run.lock"), b"prior heartbeat bytes")?;
    let instant = future();
    let sealed = preview(root.path(), instant);
    let retained = surviving_paths(&sealed.view);
    let approval = sealed.view.approval_fingerprint.as_deref().unwrap();
    let report = apply_at(root.path(), approval, instant, true, Limits::default())?;
    assert!(report.complete);
    assert_eq!(
        (
            report.selected_bundles,
            report.attempted_bundles,
            report.removed_bundles
        ),
        (2, 2, 2)
    );
    assert_eq!(
        report.removed_logical_bytes,
        2 * b"derived evidence".len() as u64
    );
    assert!(report.errors.is_empty());
    assert_eq!(retained.len(), 1);
    assert_eq!(
        fs::read(retained[0].join("index/MANIFEST"))?,
        b"derived evidence"
    );
    assert_eq!(
        fs::read(root.path().join("agent_search.db"))?,
        b"unreadable archive sentinel"
    );
    assert_eq!(
        fs::read(root.path().join("index/v9-quill/MANIFEST"))?,
        b"live"
    );
    assert_eq!(
        fs::read(root.path().join("index-run.lock"))?,
        b"prior heartbeat bytes"
    );
    // Replaying an obsolete approval must not silently select a different plan.
    assert!(matches!(
        apply_at(root.path(), approval, instant, true, Limits::default()),
        Err(CleanupError::ApprovalChanged)
    ));
    let new_plan = preview(root.path(), instant);
    let no_op = apply_at(
        root.path(),
        new_plan.view.approval_fingerprint.as_deref().unwrap(),
        instant,
        true,
        Limits::default(),
    )?;
    assert!(no_op.complete);
    assert_eq!(no_op.attempted_bundles, 0);
    Ok(())
}

#[test]
fn changed_evidence_or_new_bundles_refuse_before_any_quarantine_mutation() -> TestResult {
    let root = tempfile::tempdir()?;
    populated(root.path());
    let instant = future();
    let sealed = preview(root.path(), instant);
    let paths: Vec<_> = sealed
        .view
        .inventory
        .artifacts
        .iter()
        .map(|a| a.path.clone())
        .collect();
    fs::write(paths[0].join("index/new-data"), b"new evidence")?;
    assert!(matches!(
        apply_at(
            root.path(),
            sealed.view.approval_fingerprint.as_deref().unwrap(),
            instant,
            true,
            Limits::default()
        ),
        Err(CleanupError::ApprovalChanged)
    ));
    for path in &paths {
        assert!(path.join("index/MANIFEST").is_file());
    }
    let second = preview(root.path(), instant);
    let added = bundle(root.path(), "000004");
    assert!(matches!(
        apply_at(
            root.path(),
            second.view.approval_fingerprint.as_deref().unwrap(),
            instant,
            true,
            Limits::default()
        ),
        Err(CleanupError::ApprovalChanged)
    ));
    assert!(added.join("index/MANIFEST").is_file());
    for path in &paths {
        assert!(path.join("index/MANIFEST").is_file());
    }
    Ok(())
}

#[test]
fn approval_is_scoped_to_the_directory_and_retention_decisions() -> TestResult {
    let first = tempfile::tempdir()?;
    let second = tempfile::tempdir()?;
    populated(first.path());
    populated(second.path());
    let instant = future();
    let old = preview(first.path(), instant);
    assert!(matches!(
        apply_at(
            second.path(),
            old.view.approval_fingerprint.as_deref().unwrap(),
            instant,
            true,
            Limits::default()
        ),
        Err(CleanupError::ApprovalChanged)
    ));
    let recent = preview(first.path(), SystemTime::now());
    assert!(
        recent
            .view
            .inventory
            .artifacts
            .iter()
            .all(|a| a.disposition != Disposition::Eligible)
    );
    assert!(matches!(
        apply_at(
            first.path(),
            recent.view.approval_fingerprint.as_deref().unwrap(),
            instant,
            true,
            Limits::default()
        ),
        Err(CleanupError::ApprovalChanged)
    ));
    assert_eq!(
        preview(first.path(), instant)
            .view
            .inventory
            .artifacts
            .len(),
        3
    );
    assert_eq!(
        preview(second.path(), instant)
            .view
            .inventory
            .artifacts
            .len(),
        3
    );
    Ok(())
}

#[test]
fn incomplete_and_disabled_plans_have_no_approval_and_cannot_apply() -> TestResult {
    let root = tempfile::tempdir()?;
    populated(root.path());
    let instant = future();
    let sealed = preview(root.path(), instant);
    let approval = sealed.view.approval_fingerprint.as_deref().unwrap();
    let limits = Limits {
        tree_entries: 1,
        ..Default::default()
    };
    let partial = seal(root.path(), instant, true, limits)?;
    assert!(!partial.view.inventory.complete);
    assert!(partial.view.approval_fingerprint.is_none());
    assert!(matches!(
        apply_at(root.path(), approval, instant, true, limits),
        Err(CleanupError::Incomplete)
    ));
    let disabled = seal(root.path(), instant, false, Limits::default())?;
    assert!(disabled.view.approval_fingerprint.is_none());
    assert!(matches!(
        apply_at(root.path(), approval, instant, false, Limits::default()),
        Err(CleanupError::Disabled)
    ));
    assert_eq!(
        preview(root.path(), instant).view.inventory.artifacts.len(),
        3
    );
    Ok(())
}

#[test]
fn malformed_approval_does_not_even_create_the_lock() -> TestResult {
    let root = tempfile::tempdir()?;
    for value in [
        "",
        "yes",
        "lq-cleanup-v1-",
        "lq-cleanup-v1-../index",
        &"f".repeat(64),
    ] {
        assert!(matches!(
            apply_at(root.path(), value, future(), true, Limits::default()),
            Err(CleanupError::InvalidApproval)
        ));
    }
    assert_eq!(fs::read_dir(root.path())?.count(), 0);
    Ok(())
}

#[test]
fn indexer_lock_conflict_blocks_cleanup_and_releases_without_touching_heartbeat() -> TestResult {
    let root = tempfile::tempdir()?;
    populated(root.path());
    let instant = future();
    let sealed = preview(root.path(), instant);
    let path = root.path().join("index-run.lock");
    fs::write(&path, b"active indexer")?;
    let held = OpenOptions::new().read(true).write(true).open(&path)?;
    fs2::FileExt::try_lock_exclusive(&held)?;
    let approval = sealed.view.approval_fingerprint.as_deref().unwrap();
    assert!(matches!(
        apply_at(root.path(), approval, instant, true, Limits::default()),
        Err(CleanupError::Busy)
    ));
    assert_eq!(
        preview(root.path(), instant).view.inventory.artifacts.len(),
        3
    );
    fs2::FileExt::unlock(&held)?;
    assert!(apply_at(root.path(), approval, instant, true, Limits::default())?.complete);
    assert_eq!(fs::read(path)?, b"active indexer");
    Ok(())
}

#[test]
fn a_late_target_change_reports_partial_execution_and_stops() -> TestResult {
    let root = tempfile::tempdir()?;
    populated(root.path());
    let instant = future();
    let lease = lease::Lease::acquire(root.path())?;
    let sealed = preview(root.path(), instant);
    let eligible: Vec<_> = sealed
        .view
        .inventory
        .artifacts
        .iter()
        .filter(|a| a.disposition == Disposition::Eligible)
        .map(|a| a.path.clone())
        .collect();
    let retained = surviving_paths(&sealed.view);
    assert_eq!(eligible.len(), 2);
    // Simulate an uncoordinated change AFTER approval comparison and BEFORE a
    // later target's recheck, without replacing the production mutation helper.
    fs::write(eligible[1].join("index/late-change"), b"retain this")?;
    let report = execute(sealed, &lease, instant, Limits::default());
    assert!(!report.complete);
    assert_eq!((report.attempted_bundles, report.removed_bundles), (2, 1));
    assert_eq!(report.errors.len(), 1);
    assert!(!eligible[0].exists());
    assert_eq!(
        fs::read(eligible[1].join("index/late-change"))?,
        b"retain this"
    );
    assert_eq!(
        fs::read(retained[0].join("index/MANIFEST"))?,
        b"derived evidence"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn symlink_or_hardlinked_lock_is_never_used_or_overwritten() -> TestResult {
    let root = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;
    populated(root.path());
    let sentinel = outside.path().join("sentinel");
    fs::write(&sentinel, b"outside bytes")?;
    let path = root.path().join("index-run.lock");
    std::os::unix::fs::symlink(&sentinel, &path)?;
    let instant = future();
    let sealed = preview(root.path(), instant);
    assert!(matches!(
        apply_at(
            root.path(),
            sealed.view.approval_fingerprint.as_deref().unwrap(),
            instant,
            true,
            Limits::default()
        ),
        Err(CleanupError::Io(_))
    ));
    // Use another data directory for the hard-link case, leaving the symlink intact.
    let other = tempfile::tempdir()?;
    populated(other.path());
    fs::hard_link(&sentinel, other.path().join("index-run.lock"))?;
    let sealed = preview(other.path(), instant);
    assert!(matches!(
        apply_at(
            other.path(),
            sealed.view.approval_fingerprint.as_deref().unwrap(),
            instant,
            true,
            Limits::default()
        ),
        Err(CleanupError::Io(_))
    ));
    assert_eq!(fs::read(sentinel)?, b"outside bytes");
    assert_eq!(
        preview(root.path(), instant).view.inventory.artifacts.len(),
        3
    );
    assert_eq!(
        preview(other.path(), instant)
            .view
            .inventory
            .artifacts
            .len(),
        3
    );
    Ok(())
}

#[test]
fn existing_empty_plan_applies_as_noop_without_opening_archive() -> TestResult {
    let root = tempfile::tempdir()?;
    let database = root.path().join("agent_search.db");
    fs::write(&database, b"not a database")?;
    let instant = future();
    let sealed = preview(root.path(), instant);
    let report = apply_at(
        root.path(),
        sealed.view.approval_fingerprint.as_deref().unwrap(),
        instant,
        true,
        Limits::default(),
    )?;
    assert!(report.complete);
    assert_eq!(
        (
            report.selected_bundles,
            report.attempted_bundles,
            report.removed_bundles
        ),
        (0, 0, 0)
    );
    assert_eq!(fs::read(database)?, b"not a database");
    // A fresh exclusive owner can acquire the retained lock inode after return.
    let file = File::open(root.path().join("index-run.lock"))?;
    fs2::FileExt::try_lock_exclusive(&file)?;
    Ok(())
}
