//! Windows source identity and mutation evidence (if17v / GH #512).
//!
//! Size and LastWriteTime alone miss replacement and same-size rewrites when a
//! producer restores mtime. Query the 128-bit file ID and ChangeTime through one
//! attribute-only handle instead. This is a filesystem change detector, not a
//! content digest or a lock over the connector's later parse. Coarse timestamps
//! and an actor able to restore all change metadata are outside that guarantee.
//! Unsupported or unstable evidence disables reuse; it never falls back to the
//! old size/mtime-only certificate. No transcript body or directory is scanned.

use serde_json::{Value, json};
use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::time::UNIX_EPOCH;

const FILE_READ_ATTRIBUTES: u32 = 0x0080;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const FILE_SHARE_READ_WRITE_DELETE: u32 = 0x0007;
const SECURITY_IDENTIFICATION: u32 = 0x0001_0000;

// Win32 FILE_BASIC_INFO and FILE_ID_INFO, as documented in winbase.h.
// Keep these bindings local: no new dependency or nightly feature is needed.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct BasicInfo {
    creation_time: i64,
    _last_access_time: i64,
    last_write_time: i64,
    change_time: i64,
    file_attributes: u32,
}

impl BasicInfo {
    fn same_change_state(self, other: Self) -> bool {
        // Normal reads can advance LastAccessTime and must not invalidate a
        // completed parse. Do not serialize or compare that field.
        self.creation_time == other.creation_time
            && self.last_write_time == other.last_write_time
            && self.change_time == other.change_time
            && self.file_attributes == other.file_attributes
    }
}

#[repr(C)]
#[derive(Default)]
struct IdInfo {
    volume_serial_number: u64,
    file_id: [u8; 16],
}

const _: () = assert!(std::mem::size_of::<BasicInfo>() == 40);
const _: () = assert!(std::mem::size_of::<IdInfo>() == 24);

// SAFETY: exact Win32 ABI: HANDLE/LPVOID are pointers, the information class
// is a C enum (32 bits), DWORD is u32 and BOOL is i32. Each caller supplies the
// corresponding repr(C) buffer and keeps both it and the File alive.
#[allow(unsafe_code)]
#[link(name = "kernel32")]
unsafe extern "system" {
    #[link_name = "GetFileInformationByHandleEx"]
    fn get_file_information_by_handle_ex(
        file: *mut c_void,
        class: i32,
        information: *mut c_void,
        bytes: u32,
    ) -> i32;
}

fn basic_info(file: &File) -> io::Result<BasicInfo> {
    let mut info = BasicInfo::default();
    // SAFETY: FileBasicInfo (0) writes exactly FILE_BASIC_INFO into this live,
    // correctly aligned buffer; the borrowed handle is neither closed nor kept.
    #[allow(unsafe_code)]
    let ok = unsafe {
        get_file_information_by_handle_ex(file.as_raw_handle(), 0, (&raw mut info).cast(), 40)
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(info)
    }
}

fn id_info(file: &File) -> io::Result<IdInfo> {
    let mut info = IdInfo::default();
    // SAFETY: FileIdInfo (18) writes exactly FILE_ID_INFO. The full 128-bit ID
    // is retained, including on ReFS; no lossy 64-bit fallback is admitted.
    #[allow(unsafe_code)]
    let ok = unsafe {
        get_file_information_by_handle_ex(file.as_raw_handle(), 18, (&raw mut info).cast(), 24)
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(info)
    }
}

fn open_observation(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ_WRITE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .security_qos_flags(SECURITY_IDENTIFICATION)
        .open(path)
}

pub(super) fn file_observation(path: &Path, encoded_path: &str) -> Option<Value> {
    let file = match open_observation(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Some(json!({"path": encoded_path, "absent": true}));
        }
        Err(_) => return None,
    };
    observation_from_handle(&file, encoded_path)
}

fn observation_from_handle(file: &File, encoded_path: &str) -> Option<Value> {
    let before = basic_info(file).ok()?;
    let identity = id_info(file).ok()?;
    let metadata = file.metadata().ok()?;
    let after = basic_info(file).ok()?;
    // All fields must describe one stable observation of the same open file.
    // Failed probes AFTER opening are unknown evidence, not proof of absence.
    if !(metadata.is_file() || metadata.is_dir())
        || !before.same_change_state(after)
        || after.change_time <= 0
        || identity.file_id == [0; 16]
        || u64::try_from(after.last_write_time).ok() != Some(metadata.last_write_time())
        || u64::try_from(after.creation_time).ok() != Some(metadata.creation_time())
        || after.file_attributes != metadata.file_attributes()
    {
        return None;
    }
    Some(json!({
        "path": encoded_path,
        "size": metadata.len(),
        "mtime_ns": metadata.modified().ok()?.duration_since(UNIX_EPOCH)
            .ok()?.as_nanos().to_string(),
        "windows": {
            "volume_serial": identity.volume_serial_number,
            "file_id": hex::encode(identity.file_id),
            "change_time_100ns": after.change_time.to_string(),
            "creation_time_100ns": after.creation_time.to_string(),
            "attributes": after.file_attributes,
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::super::{DependencyObservations, file_observation, ledger_matches};
    use super::*;
    use std::fs;
    use std::io::{Read, Write};
    use std::time::{Duration, SystemTime};

    const CONTRACT: &str = "windows-source-evidence-test";

    fn saved(path: &Path, dependencies: Vec<Value>) -> String {
        json!({"producer_contract": CONTRACT,
            "primary": file_observation(path).expect("native file identity and change time"),
            "dependencies": dependencies})
        .to_string()
    }

    fn rewrite_preserving_mtime(path: &Path, bytes: &[u8], modified: SystemTime) {
        // Separate native timestamp ticks; no timing or throughput is measured.
        std::thread::sleep(Duration::from_millis(20));
        let mut writer = OpenOptions::new().write(true).open(path).unwrap();
        writer.write_all(bytes).unwrap();
        writer.sync_all().unwrap();
        writer.set_modified(modified).unwrap();
        drop(writer);
    }

    #[test]
    fn windows_attribute_only_observation_is_stable_and_cannot_read_body() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("source 雪.jsonl");
        fs::write(&path, b"private transcript").unwrap();
        let mut handle = open_observation(&path).unwrap();
        let expected = observation_from_handle(&handle, path.to_str().unwrap()).unwrap();
        assert_eq!(file_observation(&path), Some(expected.clone()));
        assert_eq!(file_observation(&path), Some(expected.clone()));
        assert_eq!(expected["windows"]["file_id"].as_str().unwrap().len(), 32);
        assert!(expected["windows"]["change_time_100ns"].is_string());
        assert!(expected["windows"].get("last_access_time").is_none());
        assert_eq!(
            handle.read(&mut [0; 1]).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(file_observation(root.path()).unwrap()["windows"].is_object());
        assert_eq!(fs::read(&path).unwrap(), b"private transcript");
    }

    #[test]
    fn windows_same_size_primary_rewrite_with_restored_mtime_invalidates_reuse() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("primary.jsonl");
        fs::write(&path, b"old transcript").unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        let before = file_observation(&path).unwrap();
        let row = saved(&path, vec![file_observation(root.path()).unwrap()]);
        assert!(ledger_matches(&row, &path, false, CONTRACT));
        rewrite_preserving_mtime(&path, b"new transcript", modified);
        let after = file_observation(&path).unwrap();
        assert_eq!(before["size"], after["size"]);
        assert_eq!(before["mtime_ns"], after["mtime_ns"]);
        assert_eq!(before["windows"]["file_id"], after["windows"]["file_id"]);
        assert_ne!(
            before["windows"]["change_time_100ns"],
            after["windows"]["change_time_100ns"]
        );
        for observes_parent in [false, true] {
            assert!(!ledger_matches(&row, &path, observes_parent, CONTRACT));
        }
        let renewed = saved(&path, vec![file_observation(root.path()).unwrap()]);
        assert!(ledger_matches(&renewed, &path, true, CONTRACT));
        assert_eq!(fs::read(&path).unwrap(), b"new transcript");
    }

    #[test]
    fn windows_replacement_keeps_full_identity_even_at_equal_size_and_mtime() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("primary.jsonl");
        let replacement = root.path().join("replacement.jsonl");
        fs::write(&path, b"old transcript").unwrap();
        fs::write(&replacement, b"new transcript").unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        OpenOptions::new()
            .write(true)
            .open(&replacement)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        let before = file_observation(&path).unwrap();
        let row = saved(&path, vec![]);
        // Retain the original object so its ID cannot be recycled for the new one.
        let original = root.path().join("retained-original.jsonl");
        fs::rename(&path, &original).unwrap();
        fs::rename(&replacement, &path).unwrap();
        let after = file_observation(&path).unwrap();
        assert_eq!(before["size"], after["size"]);
        assert_eq!(before["mtime_ns"], after["mtime_ns"]);
        assert_ne!(before["windows"]["file_id"], after["windows"]["file_id"]);
        assert!(!ledger_matches(&row, &path, false, CONTRACT));
        assert_eq!(fs::read(original).unwrap(), b"old transcript");
        assert_eq!(fs::read(path).unwrap(), b"new transcript");
    }

    #[test]
    fn windows_sidecar_rewrite_cannot_hide_behind_a_self_contained_primary() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("primary.jsonl");
        let sidecar = root.path().join("state.json");
        fs::write(&path, b"unchanged transcript").unwrap();
        fs::write(&sidecar, b"old metadata").unwrap();
        let before = file_observation(&sidecar).unwrap();
        let row = saved(&path, vec![before.clone()]);
        assert!(ledger_matches(&row, &path, false, CONTRACT));
        let modified = fs::metadata(&sidecar).unwrap().modified().unwrap();
        rewrite_preserving_mtime(&sidecar, b"new metadata", modified);
        let after = file_observation(&sidecar).unwrap();
        assert_eq!(before["size"], after["size"]);
        assert_eq!(before["mtime_ns"], after["mtime_ns"]);
        assert!(!ledger_matches(&row, &path, false, CONTRACT));
        assert_eq!(fs::read(&path).unwrap(), b"unchanged transcript");
    }

    #[test]
    fn windows_parent_change_is_observed_without_invalidating_self_contained_sources() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("primary.jsonl");
        fs::write(&path, b"unchanged transcript").unwrap();
        let modified = fs::metadata(root.path()).unwrap().modified().unwrap();
        let before = file_observation(root.path()).unwrap();
        let row = saved(&path, vec![before.clone()]);
        let captured = DependencyObservations::capture([path.clone()]);
        std::thread::sleep(Duration::from_millis(20));
        fs::write(root.path().join("new-sidecar.json"), b"metadata").unwrap();
        // Restore the directory mtime too. This opens only metadata-write access.
        OpenOptions::new()
            .access_mode(0x0100) // FILE_WRITE_ATTRIBUTES
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(root.path())
            .unwrap()
            .set_modified(modified)
            .unwrap();
        let after = file_observation(root.path()).unwrap();
        assert_eq!(before["mtime_ns"], after["mtime_ns"]);
        assert_ne!(
            before["windows"]["change_time_100ns"],
            after["windows"]["change_time_100ns"]
        );
        assert_eq!(captured.parent_for(&path), Some(&before));
        assert!(!ledger_matches(&row, &path, true, CONTRACT));
        assert!(ledger_matches(&row, &path, false, CONTRACT));
    }

    #[test]
    fn windows_legacy_size_mtime_rows_require_a_fresh_completed_parse() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("primary.jsonl");
        fs::write(&path, b"unchanged transcript").unwrap();
        let mut legacy = file_observation(&path).unwrap();
        assert!(
            legacy
                .as_object_mut()
                .unwrap()
                .remove("windows")
                .is_some()
        );
        let row = json!({"producer_contract": CONTRACT, "primary": legacy,
            "dependencies": []}).to_string();
        assert!(!ledger_matches(&row, &path, false, CONTRACT));
        let new = saved(&path, vec![]);
        assert!(ledger_matches(&new, &path, false, CONTRACT));
        assert!(!ledger_matches(&new, &path, false, "changed-producer"));
        assert_eq!(fs::read(&path).unwrap(), b"unchanged transcript");
    }

    #[test]
    fn windows_absence_is_distinct_from_failed_evidence() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("missing.jsonl");
        let absent = file_observation(&missing).unwrap();
        assert_eq!(absent, json!({"path": missing, "absent": true}));
        let row = json!({"producer_contract": CONTRACT, "primary": absent,
            "dependencies": []}).to_string();
        assert!(!ledger_matches(&row, &missing, false, CONTRACT));
        let invalid = Path::new("invalid\0path");
        assert!(file_observation(invalid).is_none());
        // An unqueryable handle must never be presented as an absent sidecar.
        let device = OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .open("NUL")
            .unwrap();
        assert!(observation_from_handle(&device, "NUL").is_none());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn windows_change_state_excludes_access_time_but_requires_mutation_fields() {
        let before = BasicInfo {
            creation_time: 1,
            _last_access_time: 2,
            last_write_time: 3,
            change_time: 4,
            file_attributes: 0x20,
        };
        assert!(before.same_change_state(BasicInfo {
            _last_access_time: 99,
            ..before
        }));
        for changed in [
            BasicInfo {
                creation_time: 99,
                ..before
            },
            BasicInfo {
                last_write_time: 99,
                ..before
            },
            BasicInfo {
                change_time: 99,
                ..before
            },
            BasicInfo {
                file_attributes: 0x10,
                ..before
            },
        ] {
            assert!(!before.same_change_state(changed));
        }
        assert_eq!(std::mem::offset_of!(BasicInfo, file_attributes), 32);
        assert_eq!(std::mem::offset_of!(IdInfo, file_id), 8);
    }
}
