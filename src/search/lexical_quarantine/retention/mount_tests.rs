//! Fault-injected mount changes at the real deletion path's two boundaries.
//! No mounts or namespaces are changed; all mutation targets are temp fixtures.

use super::*;
use std::io::Cursor;
use std::os::unix::ffi::OsStrExt;

fn seed(root: &Path, pending: bool) -> (Inventory, SystemTime, fs::File) {
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("index-run.lock"))
        .unwrap();
    fs2::FileExt::try_lock_exclusive(&lock).unwrap();
    let survivor = root.join(format!("{QUARANTINE_PREFIX}000001"));
    fs::create_dir_all(survivor.join("index")).unwrap();
    fs::write(survivor.join("index/MANIFEST"), b"newest evidence").unwrap();
    let now = SystemTime::now() + Duration::from_secs(24 * 60 * 60);
    fs::File::open(survivor.join("index/MANIFEST"))
        .unwrap()
        .set_modified(now - Duration::from_secs(12 * 60 * 60))
        .unwrap();
    let prefix = if pending {
        RECLAIM_PREFIX
    } else {
        QUARANTINE_PREFIX
    };
    let target = root.join(format!("{prefix}000002"));
    let content = if pending {
        target.join("quarantine/index")
    } else {
        target.join("index")
    };
    fs::create_dir_all(&content).unwrap();
    fs::write(content.join("MANIFEST"), b"do not traverse the mount").unwrap();
    let inventory = inspect_at(root, now, true, Limits::default()).unwrap();
    assert!(inventory.complete, "{:?}", inventory.warnings);
    assert_eq!(
        inventory
            .artifacts
            .iter()
            .filter(|a| a.disposition == Disposition::Eligible)
            .count(),
        1
    );
    (inventory, now, lock)
}

fn refuse_mount(path: &Path, work: &mut Budget) -> io::Result<()> {
    let canonical = fs::canonicalize(path)?;
    // Escape the actual temp path exactly as Linux mountinfo does.
    let mut encoded = Vec::new();
    for &byte in canonical.as_os_str().as_bytes() {
        match byte {
            b' ' => encoded.extend_from_slice(br"\040"),
            b'\t' => encoded.extend_from_slice(br"\011"),
            b'\n' => encoded.extend_from_slice(br"\012"),
            b'\\' => encoded.extend_from_slice(br"\134"),
            other => encoded.push(other),
        }
    }
    let mut table = b"1 1 8:1 / / rw - ext4 disk rw\n2 1 8:1 /outside ".to_vec();
    table.extend_from_slice(&encoded);
    table.extend_from_slice(b" rw - ext4 disk rw\n");
    mounts::check_reader(Cursor::new(table), &canonical, work)
}

#[test]
fn late_mount_refusal_before_rename_preserves_ordinary_and_interrupted_targets() {
    for pending in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (inventory, now, _lock) = seed(root.path(), pending);
        let target = inventory
            .artifacts
            .iter()
            .find(|a| a.disposition == Disposition::Eligible)
            .unwrap();
        let before = fs::read_dir(root.path()).unwrap().count();
        let error = reclaim_one_with_mount_check(
            target,
            &inventory,
            now,
            &mut Budget::new(Limits::default()),
            refuse_mount,
        )
        .unwrap_err();
        assert!(error.to_string().contains("mount point"));
        assert!(target.path.is_dir());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), before);
        let payload = if pending {
            "quarantine/index/MANIFEST"
        } else {
            "index/MANIFEST"
        };
        assert_eq!(
            fs::read(target.path.join(payload)).unwrap(),
            b"do not traverse the mount"
        );
    }
}

#[test]
fn post_rename_refusal_retains_a_recognizable_bundle_and_safe_retry_preserves_newest() {
    let root = tempfile::tempdir().unwrap();
    let (inventory, now, _lock) = seed(root.path(), false);
    let target = inventory
        .artifacts
        .iter()
        .find(|a| a.disposition == Disposition::Eligible)
        .unwrap();
    let survivor = inventory
        .artifacts
        .iter()
        .find(|a| a.disposition == Disposition::RetainNewest)
        .unwrap();
    let mut checks = Vec::new();
    let error = reclaim_one_with_mount_check(
        target,
        &inventory,
        now,
        &mut Budget::new(Limits::default()),
        |path, work| {
            checks.push(path.to_owned());
            if checks.len() == 1 {
                mounts::check(path, work)
            } else {
                refuse_mount(path, work)
            }
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("mount point"));
    assert_eq!(checks.len(), 2);
    assert!(
        !target.path.exists(),
        "rename happened before the final refusal"
    );
    let pending = &checks[1];
    assert!(
        pending
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with(RECLAIM_PREFIX)
    );
    assert_eq!(
        fs::read(pending.join("quarantine/index/MANIFEST")).unwrap(),
        b"do not traverse the mount"
    );
    assert_eq!(
        fs::read(survivor.path.join("index/MANIFEST")).unwrap(),
        b"newest evidence"
    );
    // The injected mount is gone. Re-inventory, never reuse the old approval.
    let retry = inspect_at(root.path(), now, true, Limits::default()).unwrap();
    assert!(retry.complete);
    let target = retry.artifacts.iter().find(|a| a.path == *pending).unwrap();
    assert!(target.interrupted_reclaim);
    reclaim_one(target, &retry, now, &mut Budget::new(Limits::default())).unwrap();
    assert!(!pending.exists());
    assert_eq!(
        fs::read(survivor.path.join("index/MANIFEST")).unwrap(),
        b"newest evidence"
    );
}

#[test]
fn unavailable_mount_evidence_is_a_refusal_not_an_empty_mount_table() {
    let root = tempfile::tempdir().unwrap();
    let (inventory, now, _lock) = seed(root.path(), false);
    let target = inventory
        .artifacts
        .iter()
        .find(|a| a.disposition == Disposition::Eligible)
        .unwrap();
    let error = reclaim_one_with_mount_check(
        target,
        &inventory,
        now,
        &mut Budget::new(Limits::default()),
        |_, _| {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "mountinfo unavailable",
            ))
        },
    )
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert!(target.path.is_dir());
    assert_eq!(
        fs::read(target.path.join("index/MANIFEST")).unwrap(),
        b"do not traverse the mount"
    );
}
