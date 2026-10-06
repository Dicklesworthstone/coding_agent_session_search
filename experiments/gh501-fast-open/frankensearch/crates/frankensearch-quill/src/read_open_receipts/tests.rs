use super::*;

fn fixture() -> (Binding, Identity, Receipt) {
    let binding = Binding { schema_id: 7, segment_id: 8, file_len: 100, file_xxh3: 9 };
    let identity = Identity { dev: 1, ino: 2, len: 100, mtime_s: 100, mtime_ns: 3, ctime_s: 100, ctime_ns: 4 };
    (binding, identity, Receipt { binding, identity, verified_s: 200 })
}

fn encoded_fixture() -> Vec<u8> {
    let (binding, _, receipt) = fixture();
    encode(&BTreeMap::from([(binding, receipt)]), [42; 32]).expect("encode")
}

#[test]
fn round_trip_binds_schema_identity_and_producer() {
    let (binding, _, receipt) = fixture();
    let bytes = encoded_fixture();
    assert_eq!(decode(&bytes, [42; 32]), Some(BTreeMap::from([(binding, receipt)])));
    assert!(decode(&bytes, [43; 32]).is_none());
}

#[test]
fn every_single_byte_corruption_is_rejected() {
    let good = encoded_fixture();
    for offset in 0..good.len() {
        let mut bad = good.clone(); bad[offset] ^= 1;
        assert!(decode(&bad, [42; 32]).is_none(), "byte {offset}");
    }
}

#[test]
fn every_truncation_and_oversized_book_is_rejected() {
    let good = encoded_fixture();
    for end in 0..good.len() { assert!(decode(&good[..end], [42; 32]).is_none()); }
    assert!(decode(&vec![0; MAX_BYTES + 1], [42; 32]).is_none());
    let mut extended = good; extended.push(0);
    assert!(decode(&extended, [42; 32]).is_none());
}

fn seal(body: &[u8]) -> Vec<u8> {
    let mut bytes = body.to_vec(); bytes.extend_from_slice(&Sha256::digest(body)); bytes
}

#[test]
fn checksummed_invalid_fields_and_duplicate_records_fail_closed() {
    let bytes = encoded_fixture();
    let body = &bytes[..bytes.len() - CHECKSUM_BYTES];
    let mut duplicate = body.to_vec();
    duplicate[40..44].copy_from_slice(&2_u32.to_le_bytes());
    duplicate.extend_from_slice(&body[HEADER_BYTES..]);
    assert!(decode(&seal(&duplicate), [42; 32]).is_none());
    for (word, value) in [(6, 101_u64), (8, 1_000_000_000), (10, u64::MAX)] {
        let mut bad = body.to_vec();
        bad[HEADER_BYTES + word*8..HEADER_BYTES + (word+1)*8].copy_from_slice(&value.to_le_bytes());
        assert!(decode(&seal(&bad), [42; 32]).is_none());
    }
}

#[cfg(target_os = "linux")]
fn private_dir() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).expect("chmod");
    dir
}

#[cfg(target_os = "linux")]
#[test]
fn expiry_clock_rollback_and_hits_never_renew_verification() {
    let dir = private_dir(); let (binding, identity, _) = fixture();
    let policy = Policy { max_age: Duration::from_secs(60), minimum_file_age: Duration::from_secs(10) };
    let mut first = ReceiptBook::load_at(dir.path(), policy, Some(200));
    first.keep(first.verified(binding, identity, Some(identity)).expect("receipt")); first.persist();
    for now in [200, 259, 260] {
        let mut hit = ReceiptBook::load_at(dir.path(), policy, Some(now));
        let receipt = hit.admitted(binding, identity).expect("hit");
        assert_eq!(receipt.verified_s, 200); hit.keep(receipt); hit.persist();
    }
    for now in [199, 261] {
        assert!(ReceiptBook::load_at(dir.path(), policy, Some(now)).admitted(binding, identity).is_none());
    }
}

#[cfg(target_os = "linux")]
#[test]
fn every_identity_and_binding_component_is_required() {
    let dir = private_dir(); let (binding, identity, _) = fixture();
    let mut book = ReceiptBook::load_at(dir.path(), Policy::default(), Some(200));
    book.keep(book.verified(binding, identity, Some(identity)).expect("receipt")); book.persist();
    let book = ReceiptBook::load_at(dir.path(), Policy::default(), Some(201));
    assert!(book.admitted(binding, identity).is_some());
    for changed in [Identity { dev: 9, ..identity }, Identity { ino: 9, ..identity },
        Identity { len: 9, ..identity }, Identity { mtime_s: 99, ..identity },
        Identity { mtime_ns: 9, ..identity }, Identity { ctime_s: 99, ..identity },
        Identity { ctime_ns: 9, ..identity }] {
        assert!(book.admitted(binding, changed).is_none());
    }
    for changed in [Binding { schema_id: 99, ..binding }, Binding { segment_id: 99, ..binding },
        Binding { file_len: 99, ..binding }, Binding { file_xxh3: 99, ..binding }] {
        assert!(book.admitted(changed, identity).is_none());
    }
    assert!(book.verified(binding, identity, None).is_none());
    assert!(book.verified(binding, identity, Some(Identity { ctime_ns: 9, ..identity })).is_none());
}

#[cfg(target_os = "linux")]
#[test]
fn racy_files_unknown_clock_and_invalid_directory_cannot_mint() {
    use std::os::unix::fs::PermissionsExt;
    let dir = private_dir(); let (binding, identity, _) = fixture();
    for now in [Some(100), Some(159), None] {
        let book = ReceiptBook::load_at(dir.path(), Policy::default(), now);
        assert!(book.verified(binding, identity, Some(identity)).is_none());
    }
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).expect("chmod");
    assert!(!ReceiptBook::load(dir.path(), Policy::default()).enabled());
}

#[cfg(target_os = "linux")]
#[test]
fn symlink_hardlink_world_writable_and_fifo_books_are_not_read() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let dir = private_dir(); let file = dir.path().join(BOOK);
    std::fs::write(&file, encoded_fixture()).expect("write");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    let handle = open_private_directory(dir.path()).expect("open directory");
    assert!(read_book(&handle).is_ok());
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o666)).expect("chmod");
    assert!(read_book(&handle).is_err());
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    let other = dir.path().join("other");
    std::fs::hard_link(&file, &other).expect("hard link");
    assert!(read_book(&handle).is_err());
    std::fs::remove_file(&file).expect("unlink owned fixture");
    symlink(&other, &file).expect("symlink");
    assert!(read_book(&handle).is_err());
    std::fs::remove_file(&file).expect("unlink owned fixture symlink");
    rustix::fs::mknodat(&handle, BOOK, rustix::fs::FileType::Fifo,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR, 0).expect("fifo");
    assert!(read_book(&handle).is_err()); // NONBLOCK prevents a hang
}

#[cfg(target_os = "linux")]
#[test]
fn descriptor_identity_detects_replacement_truncation_and_restored_mtime() {
    use std::io::{Seek, SeekFrom};
    let dir = private_dir(); let path = dir.path().join("segment");
    std::fs::write(&path, b"0123456789").expect("write");
    let mut file = std::fs::OpenOptions::new().read(true).write(true).open(&path).expect("open");
    let before = Identity::of_file(&file).expect("identity");
    let mtime = file.metadata().expect("metadata").modified().expect("mtime");
    std::thread::sleep(Duration::from_millis(20));
    file.seek(SeekFrom::Start(5)).expect("seek"); file.write_all(b"X").expect("write");
    file.set_times(std::fs::FileTimes::new().set_modified(mtime)).expect("restore mtime");
    let rewritten = Identity::of_file(&file).expect("rewritten");
    assert_eq!(before.len, rewritten.len);
    assert_eq!((before.mtime_s, before.mtime_ns), (rewritten.mtime_s, rewritten.mtime_ns));
    assert_ne!((before.ctime_s, before.ctime_ns), (rewritten.ctime_s, rewritten.ctime_ns));
    file.set_len(5).expect("truncate");
    assert_ne!(Identity::of_file(&file), Some(rewritten));
    let replacement = dir.path().join("replacement");
    std::fs::write(&replacement, b"0123456789").expect("replacement");
    std::fs::rename(&replacement, &path).expect("rename");
    let new = File::open(&path).expect("new handle");
    assert_ne!(Identity::of_file(&new).expect("new identity").ino, before.ino);
    assert_eq!(Identity::of_file(&file).expect("retained inode").ino, before.ino);
}
