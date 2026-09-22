//! Metadata preflight is not content validation. The existing publication
//! tests exercise the subsequent real FSVI owner and manifest-witness checks.

use super::*;
use std::io::Seek;

fn preflight(
    root: &Path,
    relative: &str,
    size: u64,
) -> Result<VectorFilePreflight, SemanticGenerationError> {
    preflight_vector_file(
        root,
        &root.canonicalize().unwrap(),
        relative,
        SemanticArtifactRole::FastVector,
        size,
    )
}

#[test]
fn preflight_does_not_read_or_authenticate_vector_contents() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("selected.fsvi");
    let bytes = b"not an FSVI image; only a retained owner can validate this";
    fs::write(&path, bytes).unwrap();
    let selected = preflight(root.path(), "selected.fsvi", bytes.len() as u64).unwrap();
    assert_eq!((&selected.file).stream_position().unwrap(), 0);
    selected.verify(root.path()).unwrap();
    assert_eq!((&selected.file).stream_position().unwrap(), 0);
    assert_eq!(fs::read(path).unwrap(), bytes);
}

#[cfg(unix)]
#[test]
fn sparse_large_vector_preflight_does_not_scan_the_declared_bytes() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("selected.fsvi");
    let file = File::create(&path).unwrap();
    let size = 1024 * 1024 * 1024 + 17;
    file.set_len(size).unwrap();
    drop(file);
    let selected = preflight(root.path(), "selected.fsvi", size).unwrap();
    selected.verify(root.path()).unwrap();
    assert_eq!((&selected.file).stream_position().unwrap(), 0);
    assert_eq!(fs::metadata(path).unwrap().len(), size);
}

#[test]
fn preflight_rejects_size_mismatch_and_missing_file() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("selected.fsvi"), b"data").unwrap();
    for size in [0, 3, 5, u64::MAX] {
        assert!(matches!(
            preflight(root.path(), "selected.fsvi", size),
            Err(SemanticGenerationError::ArtifactSizeMismatch { .. })
        ));
    }
    assert!(preflight(root.path(), "missing.fsvi", 4).is_err());
}

#[test]
fn preflight_rejects_directory_and_parent_traversal() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("directory.fsvi")).unwrap();
    assert!(preflight(root.path(), "directory.fsvi", 0).is_err());
    for path in ["../outside.fsvi", "nested/../../outside.fsvi", ""] {
        assert!(preflight(root.path(), path, 0).is_err(), "{path}");
    }
}

#[test]
fn vector_wal_is_refused_at_preflight_and_after_admission() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("selected.fsvi");
    fs::write(&path, b"data").unwrap();
    let selected = preflight(root.path(), "selected.fsvi", 4).unwrap();
    fs::write(wal_path_for(&path), b"WAL").unwrap();
    assert!(selected.verify(root.path()).is_err());
    assert!(preflight(root.path(), "selected.fsvi", 4).is_err());
}

#[test]
fn file_growth_is_refused_when_closing_the_admission_handoff() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("selected.fsvi");
    fs::write(&path, b"data").unwrap();
    let selected = preflight(root.path(), "selected.fsvi", 4).unwrap();
    OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_len(8)
        .unwrap();
    assert!(selected.verify(root.path()).is_err());
}

#[cfg(unix)]
#[test]
fn same_bytes_in_a_replacement_file_do_not_satisfy_the_open_file_identity() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("selected.fsvi");
    fs::write(&path, b"data").unwrap();
    let selected = preflight(root.path(), "selected.fsvi", 4).unwrap();
    fs::rename(&path, root.path().join("retained.fsvi")).unwrap();
    fs::write(&path, b"data").unwrap();
    assert!(selected.verify(root.path()).is_err());
    // The failed handoff neither changes nor deletes either file.
    assert_eq!(fs::read(&path).unwrap(), b"data");
    assert_eq!(
        fs::read(root.path().join("retained.fsvi")).unwrap(),
        b"data"
    );
}

#[cfg(unix)]
#[test]
fn symlink_files_directories_and_dangling_wals_are_refused() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("real")).unwrap();
    fs::write(root.path().join("real/selected.fsvi"), b"data").unwrap();
    symlink("real", root.path().join("linked")).unwrap();
    symlink("real/selected.fsvi", root.path().join("linked.fsvi")).unwrap();
    assert!(preflight(root.path(), "linked/selected.fsvi", 4).is_err());
    assert!(preflight(root.path(), "linked.fsvi", 4).is_err());
    let path = root.path().join("real/selected.fsvi");
    symlink("nonexistent", wal_path_for(&path)).unwrap();
    assert!(preflight(root.path(), "real/selected.fsvi", 4).is_err());
}

#[cfg(unix)]
#[test]
fn hard_links_are_refused_before_and_after_owner_admission() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("selected.fsvi");
    fs::write(&path, b"data").unwrap();
    let selected = preflight(root.path(), "selected.fsvi", 4).unwrap();
    fs::hard_link(&path, root.path().join("alias.fsvi")).unwrap();
    assert!(selected.verify(root.path()).is_err());
    assert!(preflight(root.path(), "selected.fsvi", 4).is_err());
    assert!(preflight(root.path(), "alias.fsvi", 4).is_err());
}

#[test]
fn nested_regular_vector_keeps_its_selected_identity() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("fast/shard-0")).unwrap();
    fs::write(root.path().join("fast/shard-0/selected.fsvi"), b"data").unwrap();
    let selected = preflight(root.path(), "fast/shard-0/selected.fsvi", 4).unwrap();
    selected.verify(root.path()).unwrap();
    assert_eq!((&selected.file).stream_position().unwrap(), 0);
}
