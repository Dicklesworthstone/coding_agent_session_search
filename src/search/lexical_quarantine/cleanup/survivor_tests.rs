//! The approved executor and startup retention share the same survivor fence.
//! All paths and locks here belong to isolated temporary filesystem fixtures.

use super::*;

fn bundle(root: &Path, suffix: &str) -> PathBuf {
    let path = root.join("index").join(format!("{QUARANTINE_PREFIX}{suffix}"));
    fs::create_dir_all(path.join("index")).unwrap();
    fs::write(path.join("index/MANIFEST"), b"failed rebuild candidate").unwrap();
    path
}

#[test]
fn approved_executor_stops_when_its_protected_evidence_disappears() {
    let root = tempfile::tempdir().unwrap();
    let lease = lease::Lease::acquire(root.path()).unwrap();
    for suffix in ["000001", "000002", "000003"] {
        bundle(root.path(), suffix);
    }
    let now = SystemTime::now() + Duration::from_secs(24 * 60 * 60);
    let sealed = seal(root.path(), now, true, Limits::default()).unwrap();
    let survivor = sealed.view.inventory.artifacts.iter()
        .find(|a| a.disposition == Disposition::RetainNewest).unwrap().path.clone();
    let targets: Vec<_> = sealed.view.inventory.artifacts.iter()
        .filter(|a| a.disposition == Disposition::Eligible).map(|a| a.path.clone()).collect();
    fs::rename(survivor, root.path().join("operator-saved-evidence")).unwrap();
    let report = execute(sealed, &lease, now, Limits::default());
    assert!(!report.complete);
    assert_eq!(report.selected_bundles, 2);
    assert_eq!(report.attempted_bundles, 1);
    assert_eq!(report.removed_bundles, 0);
    assert_eq!(report.removed_logical_bytes, 0);
    assert_eq!(report.errors.len(), 1);
    assert!(report.errors[0].contains("protected rebuild evidence"));
    for target in targets {
        assert_eq!(fs::read(target.join("index/MANIFEST")).unwrap(), b"failed rebuild candidate");
    }
}

#[test]
fn repeated_old_quarantine_batches_are_reclaimed_while_newest_and_recent_survive() {
    let root = tempfile::tempdir().unwrap();
    let now = SystemTime::now() + Duration::from_secs(24 * 60 * 60);
    let newest = bundle(root.path(), "000001");
    let recent = bundle(root.path(), "000002");
    for (path, age) in [(&newest, 60 * 60), (&recent, 3 * 60 * 60)] {
        fs::File::open(path.join("index/MANIFEST")).unwrap()
            .set_modified(now - Duration::from_secs(age)).unwrap();
    }
    for batch in 0..3 {
        for offset in 0..4 {
            bundle(root.path(), &format!("{:06}", 10 + batch * 4 + offset));
        }
        let plan = seal(root.path(), now, true, Limits::default()).unwrap().view;
        assert!(plan.inventory.complete);
        assert_eq!(plan.inventory.artifacts.len(), 6);
        let report = apply_at(root.path(), plan.approval_fingerprint.as_deref().unwrap(),
            now, true, Limits::default()).unwrap();
        assert!(report.complete, "{:?}", report.errors);
        assert_eq!(report.selected_bundles, 4);
        assert_eq!(report.removed_bundles, 4);
        assert_eq!(report.retained_bundles, 2);
        let after = inspect_at(root.path(), now, true, Limits::default()).unwrap();
        assert!(after.complete);
        assert_eq!(after.artifacts.len(), 2);
        assert_eq!(after.eligible_bytes, 0);
        assert_eq!(after.artifacts.iter().find(|a| a.path == newest).unwrap().disposition, Disposition::RetainNewest);
        assert_eq!(after.artifacts.iter().find(|a| a.path == recent).unwrap().disposition, Disposition::RetainRecent);
        for path in [&newest, &recent] {
            assert_eq!(fs::read(path.join("index/MANIFEST")).unwrap(), b"failed rebuild candidate");
        }
    }
}
