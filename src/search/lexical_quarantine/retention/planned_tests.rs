//! Exercise the shared automatic/approved reclamation boundary, including
//! faults after the production reservation rename. No process-wide hooks.
use super::*;

struct Fixture {
    temp: tempfile::TempDir,
    _lock: fs::File,
    newest: PathBuf,
    now: SystemTime,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let lock = fs::OpenOptions::new().create(true).truncate(false)
            .read(true).write(true).open(temp.path().join("index-run.lock")).unwrap();
        fs2::FileExt::try_lock_exclusive(&lock).unwrap();
        let now = SystemTime::now() + Duration::from_secs(86400);
        let newest = temp.path().join(format!("{QUARANTINE_PREFIX}000002"));
        fs::create_dir_all(newest.join("index")).unwrap();
        fs::write(newest.join("index/MANIFEST"), b"newest evidence").unwrap();
        fs::File::open(newest.join("index/MANIFEST")).unwrap()
            .set_modified(now - Duration::from_secs(3600)).unwrap();
        let fixture = Self { temp, _lock: lock, newest, now };
        fixture.add_target();
        fixture
    }

    fn add_target(&self) {
        let target = self.temp.path().join(format!("{QUARANTINE_PREFIX}000001"));
        fs::create_dir_all(target.join("index")).unwrap();
        fs::write(target.join("index/a"), b"first").unwrap();
        fs::write(target.join("index/z"), b"last").unwrap();
    }

    fn inventory(&self) -> Inventory {
        let inventory = inspect_at(self.temp.path(), self.now, true, Limits::default()).unwrap();
        assert!(inventory.complete, "{:?}", inventory.warnings);
        inventory
    }

    fn assert_newest(&self) {
        assert_eq!(fs::read(self.newest.join("index/MANIFEST")).unwrap(), b"newest evidence");
    }
}

fn target(inventory: &Inventory) -> &Artifact {
    inventory.artifacts.iter().find(|a| a.disposition == Disposition::Eligible).unwrap()
}

#[test]
fn late_unexpected_contents_are_retained_instead_of_recursively_swept() {
    let fixture = Fixture::new();
    let inventory = fixture.inventory();
    let mut reservation = None;
    let result = reclaim_one_with_mount_check(target(&inventory), &inventory, fixture.now,
        &mut Budget::new(Limits::default()), |path, budget| {
            mounts::check(path, budget)?;
            if path.file_name().unwrap().to_string_lossy().starts_with(RECLAIM_PREFIX) {
                fs::write(path.join("quarantine/operator-notes"), b"not approved")?;
                reservation = Some(path.to_owned());
            }
            Ok(())
        });
    assert!(result.is_err());
    let reservation = reservation.unwrap();
    assert_eq!(fs::read(reservation.join("quarantine/operator-notes")).unwrap(), b"not approved");
    let after = inspect_at(fixture.temp.path(), fixture.now, true, Limits::default()).unwrap();
    assert!(!after.complete);
    assert_eq!(after.eligible_bytes, 0);
    fixture.assert_newest();
}

#[test]
fn after_rename_budget_stop_leaves_a_retryable_pending_bundle_and_no_false_removal_count() {
    let fixture = Fixture::new();
    let inventory = fixture.inventory();
    let mut reservation = None;
    let mut budget = Budget::new(Limits::default());
    budget.removal_entries = budget.limits.tree_entries - 1;
    let result = reclaim_one_with_mount_check(target(&inventory), &inventory, fixture.now,
        &mut budget, |path, budget| {
            mounts::check(path, budget)?;
            if path.file_name().unwrap().to_string_lossy().starts_with(RECLAIM_PREFIX) {
                reservation = Some(path.to_owned());
            }
            Ok(())
        });
    assert!(result.unwrap_err().to_string().contains("removal-entry"));
    let reservation = reservation.unwrap();
    assert!(!reservation.join("quarantine/index/z").exists());
    assert_eq!(fs::read(reservation.join("quarantine/index/a")).unwrap(), b"first");
    let after = fixture.inventory();
    assert!(target(&after).interrupted_reclaim);
    // A fresh locked sweep re-observes the remaining tree. The fixture clock
    // is advanced; production still honors the recent-evidence hold-down.
    let retried = reclaim(fixture.temp.path(), fixture.now, true, Limits::default());
    assert!(retried.errors.is_empty(), "{:?}", retried.errors);
    assert_eq!(retried.removed, 1);
    assert_eq!(retried.logical_bytes, 5);
    assert!(!reservation.exists());
    fixture.assert_newest();
}

#[test]
fn a_late_symlinked_parent_cannot_redirect_removal_outside_the_bundle() {
    let fixture = Fixture::new();
    let outside = fixture.temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("z"), b"outside sentinel").unwrap();
    let inventory = fixture.inventory();
    let mut saved = None;
    let result = reclaim_one_with_mount_check(target(&inventory), &inventory, fixture.now,
        &mut Budget::new(Limits::default()), |path, budget| {
            mounts::check(path, budget)?;
            if path.file_name().unwrap().to_string_lossy().starts_with(RECLAIM_PREFIX) {
                let index = path.join("quarantine/index");
                let original = path.join("quarantine/saved-index");
                fs::rename(&index, &original)?;
                std::os::unix::fs::symlink(&outside, &index)?;
                saved = Some(original);
            }
            Ok(())
        });
    assert!(result.is_err());
    assert_eq!(fs::read(outside.join("z")).unwrap(), b"outside sentinel");
    assert_eq!(fs::read(saved.unwrap().join("z")).unwrap(), b"last");
    fixture.assert_newest();
}

#[test]
fn repeated_failures_reclaim_complete_bundles_and_leave_no_empty_reservations() {
    let fixture = Fixture::new();
    for run in 0..3 {
        if run != 0 { fixture.add_target(); }
        let report = reclaim(fixture.temp.path(), fixture.now, true, Limits::default());
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.removed, 1);
        assert_eq!(report.logical_bytes, 9);
        let remaining = fixture.inventory();
        assert_eq!(remaining.artifacts.len(), 1);
        assert_eq!(remaining.artifacts[0].path, fixture.newest);
        fixture.assert_newest();
    }
}
