//! A cleanup plan must keep real evidence, not an empty directory shell. The
//! protected bundle is checked again before each mutation so a stale selection
//! cannot discard the last remaining candidate after its survivor is moved.
//! This supplements, never replaces, index-run.lock. Trusted parents and the
//! usual limitation on racing non-cooperating filesystem writers still apply.

use super::*;

pub(super) fn validate_selection(artifact: &Artifact, inventory: &Inventory) -> io::Result<()> {
    if !inventory.complete
        || !inventory.automatic_retention_enabled
        || !artifact.size_complete
        || artifact.disposition != Disposition::Eligible
        || artifact.metadata_fingerprint.is_none()
        || !inventory.artifacts.iter().any(|selected| {
            selected.path == artifact.path
                && selected.interrupted_reclaim == artifact.interrupted_reclaim
                && selected.disposition == Disposition::Eligible
                && selected.size_complete
                && selected.metadata_fingerprint == artifact.metadata_fingerprint
        })
    {
        return Err(io::Error::other("quarantine is not an eligible member of a complete enabled inventory"));
    }
    Ok(())
}

pub(super) fn recheck(inventory: &Inventory, now: SystemTime, budget: &mut Budget) -> io::Result<()> {
    let mut survivors = inventory.artifacts.iter()
        .filter(|artifact| artifact.disposition == Disposition::RetainNewest);
    let Some(survivor) = survivors.next() else {
        // A sweep containing only empty reservations/shells needs no diagnostic
        // survivor. A file-bearing bundle can never use this exception.
        return if inventory.artifacts.iter().any(|artifact| artifact.has_evidence) {
            Err(io::Error::other("quarantine inventory has evidence but no protected survivor"))
        } else {
            Ok(())
        };
    };
    if survivors.next().is_some() || !survivor.has_evidence || !survivor.size_complete {
        return Err(io::Error::other("quarantine inventory has an ambiguous protected survivor"));
    }
    let current = observe(&survivor.path, survivor.interrupted_reclaim, now, budget)
        .map_err(|error| io::Error::other(format!("protected rebuild evidence cannot be rechecked: {error}")))?;
    if !current.has_evidence
        || current.metadata_fingerprint.is_none()
        || current.metadata_fingerprint != survivor.metadata_fingerprint
    {
        return Err(io::Error::other("protected rebuild evidence changed since inventory; cleanup stopped"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lock(root: &Path) -> fs::File {
        let file = fs::OpenOptions::new().read(true).write(true).create(true)
            .truncate(false).open(root.join("index-run.lock")).unwrap();
        fs2::FileExt::try_lock_exclusive(&file).unwrap();
        file
    }

    fn bundle(root: &Path, suffix: &str, pending: bool) -> PathBuf {
        let prefix = if pending { RECLAIM_PREFIX } else { QUARANTINE_PREFIX };
        let bundle = root.join(format!("{prefix}{suffix}"));
        let layout = if pending { bundle.join("quarantine") } else { bundle.clone() };
        fs::create_dir_all(layout.join("index")).unwrap();
        fs::write(layout.join("index/MANIFEST"), b"retained failure").unwrap();
        bundle
    }

    fn scan(root: &Path, now: SystemTime) -> Inventory {
        let plan = inspect_at(root, now, true, Limits::default()).unwrap();
        assert!(plan.complete, "{:?}", plan.warnings);
        plan
    }

    fn old_now() -> SystemTime {
        SystemTime::now() + Duration::from_secs(24 * 60 * 60)
    }

    fn chosen(plan: &Inventory) -> (&Artifact, &Artifact) {
        let target = plan.artifacts.iter().find(|a| a.disposition == Disposition::Eligible).unwrap();
        let survivor = plan.artifacts.iter().find(|a| a.disposition == Disposition::RetainNewest).unwrap();
        (target, survivor)
    }

    #[test]
    fn empty_ordinary_and_pending_index_shells_cannot_displace_real_evidence() {
        let root = tempfile::tempdir().unwrap();
        let _lock = lock(root.path());
        let evidence = bundle(root.path(), "000001", true);
        let ordinary = root.path().join(format!("{QUARANTINE_PREFIX}000002"));
        let pending = root.path().join(format!("{RECLAIM_PREFIX}000003"));
        fs::create_dir_all(ordinary.join("index/empty/nested")).unwrap();
        fs::create_dir_all(pending.join("quarantine/index/empty")).unwrap();
        let now = old_now();
        let plan = scan(root.path(), now);
        let survivor = plan.artifacts.iter().find(|a| a.disposition == Disposition::RetainNewest).unwrap();
        assert_eq!(survivor.path, evidence);
        let mut budget = Budget::new(Limits::default());
        let mut removed = 0;
        for artifact in &plan.artifacts {
            if artifact.disposition == Disposition::Eligible {
                assert!(!artifact.has_evidence);
                retention::reclaim_one(artifact, &plan, now, &mut budget).unwrap();
                removed += 1;
            }
        }
        assert_eq!(removed, 2);
        assert!(!ordinary.exists());
        assert!(!pending.exists());
        assert_eq!(fs::read(evidence.join("quarantine/index/MANIFEST")).unwrap(), b"retained failure");
    }

    #[test]
    fn moved_survivor_stops_cleanup_before_target_rename() {
        let root = tempfile::tempdir().unwrap();
        let _lock = lock(root.path());
        bundle(root.path(), "000001", false);
        bundle(root.path(), "000002", false);
        let now = old_now();
        let plan = scan(root.path(), now);
        let (target, survivor) = chosen(&plan);
        fs::rename(&survivor.path, root.path().join("saved-for-inspection")).unwrap();
        let error = retention::reclaim_one(target, &plan, now, &mut Budget::new(Limits::default())).unwrap_err();
        assert!(error.to_string().contains("protected rebuild evidence"));
        assert_eq!(fs::read(target.path.join("index/MANIFEST")).unwrap(), b"retained failure");
        assert!(!fs::read_dir(root.path()).unwrap().any(|entry| entry.unwrap().file_name().to_string_lossy().starts_with(RECLAIM_PREFIX)));
    }

    #[test]
    fn emptying_survivor_payload_is_detected_even_when_its_directory_remains() {
        let root = tempfile::tempdir().unwrap();
        let _lock = lock(root.path());
        bundle(root.path(), "000001", false);
        bundle(root.path(), "000002", false);
        let now = old_now();
        let plan = scan(root.path(), now);
        let (target, survivor) = chosen(&plan);
        fs::rename(survivor.path.join("index/MANIFEST"), root.path().join("saved-manifest")).unwrap();
        assert!(survivor.path.is_dir());
        let error = retention::reclaim_one(target, &plan, now, &mut Budget::new(Limits::default())).unwrap_err();
        assert!(error.to_string().contains("protected rebuild evidence"));
        assert_eq!(fs::read(target.path.join("index/MANIFEST")).unwrap(), b"retained failure");
    }

    #[test]
    fn each_reclamation_rechecks_the_survivor_after_an_earlier_success() {
        let root = tempfile::tempdir().unwrap();
        let _lock = lock(root.path());
        for suffix in ["000001", "000002", "000003"] {
            bundle(root.path(), suffix, false);
        }
        let now = old_now();
        let plan = scan(root.path(), now);
        let targets: Vec<_> = plan.artifacts.iter().filter(|a| a.disposition == Disposition::Eligible).collect();
        assert_eq!(targets.len(), 2);
        let survivor = chosen(&plan).1;
        let mut budget = Budget::new(Limits::default());
        retention::reclaim_one(targets[0], &plan, now, &mut budget).unwrap();
        assert!(!targets[0].path.exists());
        fs::rename(&survivor.path, root.path().join("saved-for-inspection")).unwrap();
        assert!(retention::reclaim_one(targets[1], &plan, now, &mut budget).is_err());
        assert_eq!(fs::read(targets[1].path.join("index/MANIFEST")).unwrap(), b"retained failure");
    }

    #[test]
    fn incomplete_disabled_and_unselected_targets_cannot_reach_mutation() {
        let root = tempfile::tempdir().unwrap();
        let _lock = lock(root.path());
        bundle(root.path(), "000001", false);
        bundle(root.path(), "000002", false);
        let now = old_now();
        let plan = scan(root.path(), now);
        let (target, survivor) = chosen(&plan);
        let mut incomplete = plan.clone();
        incomplete.complete = false;
        let mut disabled = plan.clone();
        disabled.automatic_retention_enabled = false;
        for refused in [&incomplete, &disabled] {
            assert!(retention::reclaim_one(target, refused, now, &mut Budget::new(Limits::default())).is_err());
        }
        assert!(retention::reclaim_one(survivor, &plan, now, &mut Budget::new(Limits::default())).is_err());
        let outside = tempfile::tempdir().unwrap();
        let path = bundle(outside.path(), "000003", false);
        let mut unselected = (*target).clone();
        unselected.path = path.clone();
        assert!(retention::reclaim_one(&unselected, &plan, now, &mut Budget::new(Limits::default())).is_err());
        assert_eq!(fs::read(path.join("index/MANIFEST")).unwrap(), b"retained failure");
        assert_eq!(fs::read(target.path.join("index/MANIFEST")).unwrap(), b"retained failure");
    }

    #[test]
    fn survivor_recheck_shares_the_target_budget_and_exhaustion_retains_both() {
        let root = tempfile::tempdir().unwrap();
        let _lock = lock(root.path());
        bundle(root.path(), "000001", false);
        bundle(root.path(), "000002", false);
        let now = old_now();
        let plan = scan(root.path(), now);
        let (target, survivor) = chosen(&plan);
        // Each tree has three entries. Five covers the target but cannot
        // complete the survivor; a renewed per-bundle budget would wrongly pass.
        let mut budget = Budget::new(Limits { tree_entries: 5, ..Default::default() });
        assert!(retention::reclaim_one(target, &plan, now, &mut budget).is_err());
        assert_eq!(fs::read(target.path.join("index/MANIFEST")).unwrap(), b"retained failure");
        assert_eq!(fs::read(survivor.path.join("index/MANIFEST")).unwrap(), b"retained failure");
    }

    #[test]
    fn empty_shell_only_inventory_is_still_reclaimable() {
        let root = tempfile::tempdir().unwrap();
        let _lock = lock(root.path());
        let path = root.path().join(format!("{QUARANTINE_PREFIX}000001"));
        fs::create_dir_all(path.join("index/empty")).unwrap();
        let now = old_now();
        let plan = scan(root.path(), now);
        assert!(!plan.artifacts[0].has_evidence);
        assert_eq!(plan.artifacts[0].disposition, Disposition::Eligible);
        retention::reclaim_one(&plan.artifacts[0], &plan, now, &mut Budget::new(Limits::default())).unwrap();
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_added_to_survivor_stops_deletion_without_following_it() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let _lock = lock(root.path());
        bundle(root.path(), "000001", false);
        bundle(root.path(), "000002", false);
        fs::write(outside.path().join("sentinel"), b"outside").unwrap();
        let now = old_now();
        let plan = scan(root.path(), now);
        let (target, survivor) = chosen(&plan);
        std::os::unix::fs::symlink(outside.path(), survivor.path.join("index/escape")).unwrap();
        assert!(retention::reclaim_one(target, &plan, now, &mut Budget::new(Limits::default())).is_err());
        assert_eq!(fs::read(target.path.join("index/MANIFEST")).unwrap(), b"retained failure");
        assert_eq!(fs::read(outside.path().join("sentinel")).unwrap(), b"outside");
    }
}
