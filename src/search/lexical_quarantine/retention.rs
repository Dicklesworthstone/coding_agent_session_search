//! Locked retention is separate from the read-only inventory capability.

use super::*;

#[derive(Debug, Default)]
struct Report {
    removed: usize,
    logical_bytes: u64,
    errors: Vec<String>,
}

impl Report {
    fn log(&self) {
        if self.removed > 0 {
            tracing::info!(
                removed_bundles = self.removed,
                logical_bytes = self.logical_bytes,
                "reclaimed older failed lexical rebuilds; newest evidence retained (GH #523)"
            );
        }
        for error in &self.errors {
            tracing::warn!(error = %error,
                "lexical quarantine retention refused or incomplete; retained evidence requires inspection");
        }
    }
}

fn reclaim(data_dir: &Path, now: SystemTime, enabled: bool, limits: Limits) -> Report {
    let inventory = match inspect_at(data_dir, now, enabled, limits) {
        Ok(inventory) => inventory,
        Err(error) => {
            return Report {
                errors: vec![error.to_string()],
                ..Default::default()
            };
        }
    };
    if !enabled {
        return Report::default();
    }
    if !inventory.complete {
        let mut errors = inventory.warnings;
        errors.extend(
            inventory
                .artifacts
                .into_iter()
                .filter(|a| !a.size_complete)
                .map(|a| format!("{}: {}", a.path.display(), a.reason)),
        );
        if errors.is_empty() {
            errors.push("quarantine inventory incomplete; no bundles reclaimed".into());
        }
        return Report {
            errors,
            ..Default::default()
        };
    }
    let mut report = Report::default();
    // One additional budget across ALL candidate/survivor rechecks. On Linux
    // planned per-entry removal shares this deadline and a sweep-wide unlink
    // limit. Synchronous filesystem calls themselves cannot be preempted.
    let mut budget = Budget::new(limits);
    for artifact in inventory
        .artifacts
        .iter()
        .filter(|a| a.disposition == Disposition::Eligible)
    {
        if let Err(error) = reclaim_one(artifact, &inventory, now, &mut budget) {
            report
                .errors
                .push(format!("{}: {error}", artifact.path.display()));
            // Do not multiply work or keep mutating after a changed/failed target.
            break;
        }
        report.removed += 1;
        report.logical_bytes = report.logical_bytes.saturating_add(artifact.size_bytes);
    }
    report
}

pub(super) fn reclaim_one(
    artifact: &Artifact,
    inventory: &Inventory,
    now: SystemTime,
    budget: &mut Budget,
) -> io::Result<()> {
    reclaim_one_with_mount_check(artifact, inventory, now, budget, mounts::check)
}

fn reclaim_one_with_mount_check(
    artifact: &Artifact,
    inventory: &Inventory,
    now: SystemTime,
    budget: &mut Budget,
    mut check_mounts: impl FnMut(&Path, &mut Budget) -> io::Result<()>,
) -> io::Result<()> {
    survivor::validate_selection(artifact, inventory)?;
    let parent = artifact
        .path
        .parent()
        .ok_or_else(|| io::Error::other("quarantine has no parent"))?;
    real_directory(parent)?;
    // Hold identities through re-observation and rename. This detects ordinary
    // replacement; the data directory and its ancestors must remain trusted.
    let parent_owner = same_file::Handle::from_path(parent)?;
    let owner = same_file::Handle::from_path(&artifact.path)?;
    #[cfg(target_os = "linux")]
    let (current, removal) = reclaim_tree::Plan::observe(artifact, now, budget)?;
    #[cfg(not(target_os = "linux"))]
    let current = observe(&artifact.path, artifact.interrupted_reclaim, now, budget)?;
    if current.metadata_fingerprint != artifact.metadata_fingerprint
        || current
            .age_seconds
            .is_none_or(|age| age < MIN_AGE.as_secs())
        || owner != same_file::Handle::from_path(&artifact.path)?
    {
        return Err(io::Error::other(
            "quarantine changed since inventory; no deletion attempted",
        ));
    }
    // Recheck the retained evidence, not only the target, immediately before
    // mutation. Another operator may have moved it after the locked inventory.
    // Both walks consume the same sweep-wide budget; exhaustion fails closed.
    survivor::recheck(inventory, now, budget)?;
    budget.check()?;
    // Re-read mountinfo after target/survivor observation, before any rename.
    // Same-device bind mounts are not ruled out by metadata fingerprints.
    check_mounts(&artifact.path, budget)?;
    let doomed = if artifact.interrupted_reclaim {
        artifact.path.clone()
    } else {
        // Reserve a fresh sibling WITHOUT replacing an existing path. Retain the
        // reservation before moving evidence so any later failure/crash leaves
        // a recognizable, retryable bundle instead of losing its ownership.
        let reserved = tempfile::Builder::new()
            .prefix(RECLAIM_PREFIX)
            .tempdir_in(parent)?
            .keep();
        fs::rename(&artifact.path, reserved.join("quarantine"))?;
        real_directory(&reserved.join("quarantine"))?;
        if owner != same_file::Handle::from_path(reserved.join("quarantine"))? {
            return Err(io::Error::other(
                "quarantine identity changed during rename; retained for inspection",
            ));
        }
        reserved
    };
    if parent_owner != same_file::Handle::from_path(parent)? {
        return Err(io::Error::other(
            "quarantine parent changed; retained for inspection",
        ));
    }
    real_directory(&doomed)?;
    // A late refusal leaves the recognized reservation intact for a safe retry.
    check_mounts(&doomed, budget)?;
    // Linux removes only the names captured by the locked recheck, using
    // no-follow directory descriptors. A changed or newly populated directory
    // leaves an interrupted reservation, never an expanded recursive deletion.
    #[cfg(target_os = "linux")]
    {
        removal.remove(&doomed, !artifact.interrupted_reclaim, &parent_owner, budget)
    }
    // Keep the existing no-symlink-following std implementation elsewhere.
    #[cfg(not(target_os = "linux"))]
    fs::remove_dir_all(&doomed)
}

/// Normal index startup calls this while holding its exclusive index-run lock,
/// before disk-headroom preflight. It never acquires a second, conflicting lock.
/// This is intentionally not a public deletion API or an unlocked timer task.
pub(crate) fn reclaim_under_index_run_lock(data_dir: &Path, now: SystemTime) {
    let enabled = match retention_enabled() {
        Ok(enabled) => enabled,
        Err(error) => {
            Report {
                errors: vec![error.to_string()],
                ..Default::default()
            }
            .log();
            return;
        }
    };
    reclaim(data_dir, now, enabled, Limits::default()).log();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle(root: &Path, suffix: &str) -> PathBuf {
        let path = root.join(format!("{QUARANTINE_PREFIX}{suffix}"));
        fs::create_dir_all(path.join("index")).unwrap();
        fs::write(path.join("index/MANIFEST"), b"derived evidence").unwrap();
        path
    }

    fn now() -> SystemTime {
        SystemTime::now() + Duration::from_secs(24 * 60 * 60)
    }

    #[test]
    fn older_bundles_are_reclaimed_and_the_newest_remains_byte_identical() {
        let root = tempfile::tempdir().unwrap();
        let a = bundle(root.path(), "000001");
        let b = bundle(&root.path().join("index"), "000002");
        let c = bundle(&root.path().join("index"), "000003");
        let instant = now();
        let plan = inspect_at(root.path(), instant, true, Limits::default()).unwrap();
        let newest = plan
            .artifacts
            .iter()
            .find(|a| a.disposition == Disposition::RetainNewest)
            .unwrap()
            .path
            .clone();
        let report = reclaim(root.path(), instant, true, Limits::default());
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.removed, 2);
        assert_eq!(report.logical_bytes, 2 * b"derived evidence".len() as u64);
        assert_eq!(
            fs::read(newest.join("index/MANIFEST")).unwrap(),
            b"derived evidence"
        );
        for path in [a, b, c] {
            assert_eq!(path.exists(), path == newest);
        }
        assert_eq!(
            reclaim(root.path(), instant, true, Limits::default()).removed,
            0
        );
    }

    #[test]
    fn recent_disabled_and_incomplete_inventories_cannot_delete() {
        let root = tempfile::tempdir().unwrap();
        let a = bundle(root.path(), "000001");
        let b = bundle(root.path(), "000002");
        for (instant, enabled, limits) in [
            (SystemTime::now(), true, Limits::default()),
            (now(), false, Limits::default()),
            (
                now(),
                true,
                Limits {
                    tree_entries: 1,
                    ..Default::default()
                },
            ),
        ] {
            assert_eq!(reclaim(root.path(), instant, enabled, limits).removed, 0);
            assert!(a.join("index/MANIFEST").is_file());
            assert!(b.join("index/MANIFEST").is_file());
        }
    }

    #[test]
    fn post_inventory_changes_are_retained_before_any_rename() {
        let root = tempfile::tempdir().unwrap();
        bundle(root.path(), "000001");
        bundle(root.path(), "000002");
        let instant = now();
        let plan = inspect_at(root.path(), instant, true, Limits::default()).unwrap();
        let artifact = plan
            .artifacts
            .iter()
            .find(|a| a.disposition == Disposition::Eligible)
            .unwrap();
        fs::write(artifact.path.join("index/new-entry"), b"changed evidence").unwrap();
        let result = reclaim_one(
            artifact,
            &plan,
            instant,
            &mut Budget::new(Limits::default()),
        );
        assert!(result.is_err());
        assert_eq!(
            fs::read(artifact.path.join("index/new-entry")).unwrap(),
            b"changed evidence"
        );
    }

    #[test]
    fn interrupted_deletion_and_empty_reservations_do_not_replace_newest_evidence() {
        let root = tempfile::tempdir().unwrap();
        let newest = bundle(root.path(), "000001");
        let pending = root.path().join(format!("{RECLAIM_PREFIX}000002"));
        fs::create_dir_all(pending.join("quarantine/index")).unwrap();
        fs::write(pending.join("quarantine/index/MANIFEST"), b"old").unwrap();
        let empty = root.path().join(format!("{RECLAIM_PREFIX}000003"));
        fs::create_dir(&empty).unwrap();
        let report = reclaim(root.path(), now(), true, Limits::default());
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.removed, 2);
        assert!(!pending.exists());
        assert!(!empty.exists());
        assert_eq!(
            fs::read(newest.join("index/MANIFEST")).unwrap(),
            b"derived evidence"
        );
    }

    #[test]
    fn live_archive_publish_backups_and_unknown_evidence_are_untouched() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("agent_search.db"), b"archive sentinel").unwrap();
        fs::create_dir_all(root.path().join("index/v9-quill")).unwrap();
        fs::write(root.path().join("index/v9-quill/MANIFEST"), b"live").unwrap();
        fs::create_dir_all(root.path().join("index/.lexical-publish-backups")).unwrap();
        fs::write(
            root.path().join("index/.lexical-publish-backups/previous"),
            b"backup",
        )
        .unwrap();
        let a = bundle(root.path(), "000001");
        bundle(root.path(), "000002");
        fs::write(a.join("operator-notes"), b"retain").unwrap();
        let report = reclaim(root.path(), now(), true, Limits::default());
        assert_eq!(report.removed, 0);
        assert!(!report.errors.is_empty());
        assert_eq!(
            fs::read(root.path().join("agent_search.db")).unwrap(),
            b"archive sentinel"
        );
        assert_eq!(
            fs::read(root.path().join("index/v9-quill/MANIFEST")).unwrap(),
            b"live"
        );
        assert_eq!(
            fs::read(root.path().join("index/.lexical-publish-backups/previous")).unwrap(),
            b"backup"
        );
        assert_eq!(fs::read(a.join("operator-notes")).unwrap(), b"retain");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_in_a_candidate_blocks_the_whole_sweep_without_following_it() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("sentinel"), b"outside").unwrap();
        let a = bundle(root.path(), "000001");
        bundle(root.path(), "000002");
        std::os::unix::fs::symlink(outside.path(), a.join("index/escape")).unwrap();
        let report = reclaim(root.path(), now(), true, Limits::default());
        assert_eq!(report.removed, 0);
        assert_eq!(
            fs::read(outside.path().join("sentinel")).unwrap(),
            b"outside"
        );
        assert!(
            fs::symlink_metadata(a.join("index/escape"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
mod mount_tests;

#[cfg(all(test, target_os = "linux"))]
mod planned_tests;
