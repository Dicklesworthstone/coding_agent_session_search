//! GH #523: interrupted cleanup must not erase the last surviving evidence.

use super::*;

fn pending(root: &Path, suffix: &str) -> PathBuf {
    let path = root.join(format!("{RECLAIM_PREFIX}{suffix}"));
    fs::create_dir_all(path.join("quarantine/index")).unwrap();
    fs::write(path.join("quarantine/index/MANIFEST"), b"failure evidence").unwrap();
    path
}

fn ordinary(root: &Path, suffix: &str) -> PathBuf {
    let path = root.join(format!("{QUARANTINE_PREFIX}{suffix}"));
    fs::create_dir_all(path.join("index")).unwrap();
    fs::write(path.join("index/MANIFEST"), b"ordinary evidence").unwrap();
    path
}

fn old_now() -> SystemTime {
    SystemTime::now() + Duration::from_secs(24 * 60 * 60)
}

fn scan(root: &Path, now: SystemTime) -> Inventory {
    let inventory = inspect_at(root, now, true, Limits::default()).unwrap();
    assert!(inventory.complete, "{:?}", inventory.warnings);
    inventory
}

// Exercise the actual shared deletion primitive with an explicit writer lease,
// a complete inventory, its exact selection, and one shared recheck budget.
fn execute_selection(root: &Path, now: SystemTime) -> usize {
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("index-run.lock"))
        .unwrap();
    fs2::FileExt::try_lock_exclusive(&lock).unwrap();
    let inventory = scan(root, now);
    let mut budget = Budget::new(Limits::default());
    let mut removed = 0;
    for artifact in &inventory.artifacts {
        if artifact.disposition == Disposition::Eligible {
            retention::reclaim_one(artifact, &inventory, now, &mut budget).unwrap();
            removed += 1;
        }
    }
    removed
}

#[test]
fn sole_interrupted_bundle_is_retained_across_repeated_sweeps() {
    let root = tempfile::tempdir().unwrap();
    let path = pending(root.path(), "000001");
    let now = old_now();
    for _ in 0..3 {
        let inventory = scan(root.path(), now);
        assert_eq!(
            inventory.artifacts[0].disposition,
            Disposition::RetainNewest
        );
        assert!(
            inventory.artifacts[0]
                .reason
                .contains("interrupted cleanup")
        );
        assert_eq!(inventory.eligible_bytes, 0);
        assert_eq!(execute_selection(root.path(), now), 0);
        assert_eq!(
            fs::read(path.join("quarantine/index/MANIFEST")).unwrap(),
            b"failure evidence"
        );
    }
}

#[test]
fn empty_reservations_do_not_displace_the_last_interrupted_evidence() {
    let root = tempfile::tempdir().unwrap();
    let evidence = pending(root.path(), "000001");
    let empty_pending = root.path().join(format!("{RECLAIM_PREFIX}000002"));
    let empty_ordinary = root.path().join(format!("{QUARANTINE_PREFIX}000003"));
    fs::create_dir(&empty_pending).unwrap();
    fs::create_dir(&empty_ordinary).unwrap();
    assert_eq!(execute_selection(root.path(), old_now()), 2);
    assert!(evidence.join("quarantine/index/MANIFEST").is_file());
    assert!(!empty_pending.exists());
    assert!(!empty_ordinary.exists());
}

#[test]
fn pending_only_sweep_reclaims_older_bundles_but_keeps_one_survivor() {
    let root = tempfile::tempdir().unwrap();
    for suffix in ["000001", "000002", "000003"] {
        pending(root.path(), suffix);
    }
    let now = old_now();
    let inventory = scan(root.path(), now);
    let survivors: Vec<_> = inventory
        .artifacts
        .iter()
        .filter(|a| a.disposition == Disposition::RetainNewest)
        .collect();
    assert_eq!(survivors.len(), 1);
    let survivor = survivors[0].path.clone();
    assert_eq!(execute_selection(root.path(), now), 2);
    assert_eq!(execute_selection(root.path(), now), 0);
    assert_eq!(
        fs::read(survivor.join("quarantine/index/MANIFEST")).unwrap(),
        b"failure evidence"
    );
}

#[test]
fn fresh_ordinary_evidence_releases_the_pending_survivor() {
    let root = tempfile::tempdir().unwrap();
    let pending = pending(root.path(), "000001");
    assert_eq!(execute_selection(root.path(), old_now()), 0);
    let ordinary = ordinary(root.path(), "000002");
    assert_eq!(execute_selection(root.path(), old_now()), 1);
    assert!(!pending.exists());
    assert_eq!(
        fs::read(ordinary.join("index/MANIFEST")).unwrap(),
        b"ordinary evidence"
    );
}

#[test]
fn recent_interrupted_evidence_is_not_reclaimed_to_meet_the_survivor_count() {
    let root = tempfile::tempdir().unwrap();
    let a = pending(root.path(), "000001");
    let b = pending(root.path(), "000002");
    let now = SystemTime::now();
    let inventory = scan(root.path(), now);
    assert_eq!(inventory.eligible_bytes, 0);
    assert_eq!(execute_selection(root.path(), now), 0);
    assert!(a.exists());
    assert!(b.exists());
}

#[test]
fn checkpoint_only_interrupted_evidence_is_a_survivor() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join(format!("{RECLAIM_PREFIX}000001"));
    fs::create_dir_all(path.join("quarantine")).unwrap();
    fs::write(
        path.join("quarantine").join(CHECKPOINT),
        b"incomplete checkpoint",
    )
    .unwrap();
    assert_eq!(execute_selection(root.path(), old_now()), 0);
    assert_eq!(
        fs::read(path.join("quarantine").join(CHECKPOINT)).unwrap(),
        b"incomplete checkpoint"
    );
}

#[test]
fn unknown_interrupted_contents_disable_the_entire_selection() {
    let root = tempfile::tempdir().unwrap();
    let path = pending(root.path(), "000001");
    pending(root.path(), "000002");
    fs::write(path.join("operator-notes"), b"retain").unwrap();
    let inventory = inspect_at(root.path(), old_now(), true, Limits::default()).unwrap();
    assert!(!inventory.complete);
    assert_eq!(inventory.eligible_bytes, 0);
    assert!(
        inventory
            .artifacts
            .iter()
            .all(|a| a.disposition == Disposition::Inspect)
    );
    assert_eq!(fs::read(path.join("operator-notes")).unwrap(), b"retain");
}
