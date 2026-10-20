use super::*;
use crate::search::lexical_quarantine::{Limits, observe_with_mount_check};
use std::fs;
use std::io::{Cursor, Read};
use std::os::unix::ffi::OsStrExt;
use std::time::{Duration, SystemTime};

fn budget() -> Budget {
    Budget::new(Limits::default())
}

fn table(mountpoint: &[u8]) -> Vec<u8> {
    // Same device as the enclosing root: a st_dev comparison is insufficient.
    let mut bytes = b"1 1 8:1 / / rw - ext4 /dev/test rw\n2 1 8:1 /outside ".to_vec();
    bytes.extend_from_slice(mountpoint);
    bytes.extend_from_slice(b" rw shared:4 future:tag - ext4 /dev/test rw\n");
    bytes
}

fn escaped(path: &[u8]) -> Vec<u8> {
    let mut encoded = Vec::new();
    for &byte in path {
        match byte {
            b' ' => encoded.extend_from_slice(br"\040"),
            b'\t' => encoded.extend_from_slice(br"\011"),
            b'\n' => encoded.extend_from_slice(br"\012"),
            b'\\' => encoded.extend_from_slice(br"\134"),
            other => encoded.push(other),
        }
    }
    encoded
}

#[test]
fn same_device_root_directory_and_file_mounts_are_refused() {
    let root = Path::new("/data/.lexical-rebuild-quarantine-000001");
    for suffix in ["", "/index", "/index/MANIFEST"] {
        let point = format!("{}{suffix}", root.display());
        let error = check_reader(Cursor::new(table(point.as_bytes())), root, &mut budget())
            .unwrap_err();
        assert!(error.to_string().contains("mount point"));
    }
}

#[test]
fn ancestor_and_sibling_mounts_do_not_make_a_bundle_a_mountpoint() {
    let root = Path::new("/data/.lexical-rebuild-quarantine-000001");
    for point in [
        "/",
        "/data",
        "/data/.lexical-rebuild-quarantine-000001-other/index",
        "/elsewhere",
    ] {
        check_reader(Cursor::new(table(point.as_bytes())), root, &mut budget()).unwrap();
    }
}

#[test]
fn kernel_escapes_and_non_utf8_paths_are_compared_without_loss() {
    let mut encoded = br"/data/a\040b\011c\012d\134e/".to_vec();
    encoded.push(0xff);
    encoded.extend_from_slice(b"/index");
    let mut decoded = b"/data/a b\tc\nd\\e/".to_vec();
    decoded.push(0xff);
    let root = Path::new(std::ffi::OsStr::from_bytes(&decoded));
    let error = check_reader(Cursor::new(table(&encoded)), root, &mut budget()).unwrap_err();
    assert!(error.to_string().contains("mount point"));
    assert!(!error.to_string().contains('\t'));
    assert!(!error.to_string().contains('\n'));
}

#[test]
fn empty_truncated_or_malformed_tables_never_mean_no_mounts() {
    let root = Path::new("/data/bundle");
    for invalid_table in [
        b"".as_slice(),
        b"\n",
        b"1 1 8:1 / / rw - ext4 disk rw", // Truncated final record.
        b"1 1 8:1 / / rw ext4 disk rw\n", // Missing separator.
        b"one 1 8:1 / / rw - ext4 disk rw\n",
        b"1 1 8:1:2 / / rw - ext4 disk rw\n",
        b"1 1 8:1 relative / rw - ext4 disk rw\n",
        b"1 1 8:1 / relative rw - ext4 disk rw\n",
        b"1 1 8:1 / /data/../bundle rw - ext4 disk rw\n",
        b"1 1 8:1 / /data/\\041 rw - ext4 disk rw\n",
        b"1 1 8:1 / /data/\\04 rw - ext4 disk rw\n",
        b"1 1 8:1 / /data/\0 rw - ext4 disk rw\n",
        b"1 1 8:1 / / rw - ext4 disk\n",
        b"1 1 8:1 / / rw - ext4 disk rw trailing\n",
    ] {
        let error = check_reader(Cursor::new(invalid_table), root, &mut budget()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{invalid_table:?}");
    }
}

struct Counted<R> {
    inner: R,
    read: usize,
}

impl<R: Read> Read for Counted<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.inner.read(buffer)?;
        self.read += count;
        Ok(count)
    }
}

#[test]
fn snapshot_and_shared_work_limits_stop_reads_instead_of_accepting_partial_tables() {
    let root = Path::new("/data/bundle");
    let mut input = Counted {
        inner: io::repeat(b'x'),
        read: 0,
    };
    let mut work = budget();
    assert!(check_reader(&mut input, root, &mut work).is_err());
    assert_eq!(input.read, MAX_SNAPSHOT_BYTES + 1);
    assert_eq!(work.mount_bytes, input.read);

    let bytes = table(b"/elsewhere");
    let mut work = budget();
    work.mount_bytes = MAX_WORK_BYTES - bytes.len();
    check_reader(Cursor::new(&bytes), root, &mut work).unwrap();
    assert_eq!(work.mount_bytes, MAX_WORK_BYTES);
    let mut next = Counted {
        inner: Cursor::new(&bytes),
        read: 0,
    };
    assert!(check_reader(&mut next, root, &mut work).is_err());
    assert_eq!(next.read, 0, "the sweep must not renew an exhausted allowance");
}

#[test]
fn read_errors_and_expired_deadlines_refuse_the_boundary() {
    struct Refused;
    impl Read for Refused {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "mountinfo unavailable",
            ))
        }
    }
    let root = Path::new("/data/bundle");
    let error = check_reader(Refused, root, &mut budget()).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    let mut expired = Budget::new(Limits {
        timeout: Duration::ZERO,
        ..Default::default()
    });
    let mut input = Counted {
        inner: Cursor::new(table(b"/elsewhere")),
        read: 0,
    };
    assert!(check_reader(&mut input, root, &mut expired).is_err());
    assert_eq!(input.read, 0);
}

#[test]
fn mounted_content_is_refused_before_any_inventory_tree_entry() {
    let root = tempfile::tempdir().unwrap();
    let bundle = root.path().join(".lexical-rebuild-quarantine-000001");
    fs::create_dir_all(bundle.join("index")).unwrap();
    let sentinel = bundle.join("index/MANIFEST");
    fs::write(&sentinel, b"external mount sentinel").unwrap();
    let canonical = fs::canonicalize(&bundle).unwrap();
    let mountpoint = canonical.join("index");
    let mut work = budget();
    let error = observe_with_mount_check(
        &bundle,
        false,
        SystemTime::now(),
        &mut work,
        |_, work| {
            let input = table(&escaped(mountpoint.as_os_str().as_bytes()));
            check_reader(Cursor::new(input), &canonical, work)
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("mount point"));
    assert_eq!(work.tree_entries, 0);
    assert_eq!(fs::read(sentinel).unwrap(), b"external mount sentinel");
}

#[test]
fn real_linux_mountinfo_refuses_proc_but_allows_an_ordinary_temporary_directory() {
    // Read-only, unprivileged production check. Never attempt to remove /proc.
    let error = check(Path::new("/proc"), &mut budget()).unwrap_err();
    assert!(error.to_string().contains("mount point"), "{error}");
    let root = tempfile::tempdir().unwrap();
    check(root.path(), &mut budget()).unwrap();
}

#[test]
fn partial_failed_reads_are_charged_to_the_same_sweep() {
    struct FailAfterPrefix(bool);
    impl Read for FailAfterPrefix {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.0 {
                return Err(io::Error::other("interrupted mountinfo read"));
            }
            if buffer.is_empty() {
                return Ok(0);
            }
            self.0 = true;
            buffer[0] = b'1';
            Ok(1)
        }
    }
    let mut work = budget();
    let error = check_reader(FailAfterPrefix(false), Path::new("/data/bundle"), &mut work)
        .unwrap_err();
    assert!(error.to_string().contains("interrupted mountinfo read"));
    assert_eq!(work.mount_bytes, 1);
}
