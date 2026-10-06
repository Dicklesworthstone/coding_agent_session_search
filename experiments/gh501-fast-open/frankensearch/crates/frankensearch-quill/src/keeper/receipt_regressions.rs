// Included inside keeper::tests, beside the actual FSLX fixture constructors.
// No live mapping is ever mutated: all snapshots are dropped before faults.

fn local_receipt_fixture() -> Result<(tempfile::TempDir, PathBuf, PathBuf, EncodedSegment), Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;
    // Positive reuse must be exercised on a supported filesystem, not silently
    // skipped in an overlayfs container. Linux CI normally provides tmpfs here.
    let root = tempfile::Builder::new().prefix("quill-gh501-").tempdir_in("/dev/shm")?;
    let index = root.path().join("index"); std::fs::create_dir(&index)?;
    let cache = root.path().join("cache"); std::fs::create_dir(&cache)?;
    std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o700))?;
    let encoded = encoded_identity_test_segment(0xea501, 0, &[Some("receipt-one"), Some("receipt-two")])?;
    let segment = index.join(canonical_segment_name(0xea501));
    std::fs::write(&segment, encoded.as_bytes())?;
    assert!(crate::read_open_receipts::supported(&File::open(&segment)?), "positive receipt tests require supported local storage");
    write_manifest(&index.join("MANIFEST"), &durable_test_manifest(1, vec![manifest_segment(&encoded, 1)]))?;
    // The production window is 60 s. Tests set it to zero, but the proof clock
    // is conservatively rounded down to seconds, so cross that first tick.
    std::thread::sleep(Duration::from_millis(1100));
    Ok((root, index, cache, encoded))
}

fn eager_local_open(index: &Path, cache: &Path) -> Result<KeeperSnapshot, KeeperError> {
    KeeperSnapshot::open_local_receipts_once(index, DEFAULT_SCHEMA, cache,
        crate::read_open_receipts::Policy {
            minimum_file_age: Duration::ZERO,
            ..crate::read_open_receipts::Policy::default()
        })
}

#[test]
fn local_receipts_reuse_prefix_proof_but_plain_reader_still_hashes() -> TestResult {
    let (_root, index, cache, _) = local_receipt_fixture()?;
    let first = eager_local_open(&index, &cache)?;
    assert_eq!(first.segments()[0].authenticated_file_witness_hash_count(), 1);
    let expected = first.resolve_document_id("receipt-two")?.map(|hit| hit.global_docid);
    drop(first);
    let second = eager_local_open(&index, &cache)?;
    assert_eq!(second.segments()[0].authenticated_file_witness_hash_count(), 0);
    assert_eq!(second.resolve_document_id("receipt-two")?.map(|hit| hit.global_docid), expected);
    drop(second);
    let strict = KeeperSnapshot::open(&index, DEFAULT_SCHEMA)?;
    assert_eq!(strict.segments()[0].authenticated_file_witness_hash_count(), 1);
    Ok(())
}

#[test]
fn local_receipts_reverify_replacement_truncation_rewrite_and_restored_mtime() -> TestResult {
    for fault in ["replacement", "truncation", "rewrite", "restored-mtime"] {
        let (_root, index, cache, encoded) = local_receipt_fixture()?;
        drop(eager_local_open(&index, &cache)?);
        let admitted = eager_local_open(&index, &cache)?;
        assert_eq!(admitted.segments()[0].authenticated_file_witness_hash_count(), 0);
        drop(admitted);
        let path = index.join(canonical_segment_name(0xea501));
        if fault == "replacement" {
            let staged = index.join("staged");
            std::fs::write(&staged, encoded.as_bytes())?;
            std::fs::rename(&staged, &path)?;
            let fresh = eager_local_open(&index, &cache)?;
            assert_eq!(fresh.segments()[0].authenticated_file_witness_hash_count(), 1);
            continue;
        }
        if fault == "truncation" {
            OpenOptions::new().write(true).open(&path)?.set_len(64)?;
        } else {
            let offset = encoded.section_entries().iter()
                .find(|entry| entry.kind == SectionKind::TERMDICT).expect("termdict").offset;
            let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
            let modified = file.metadata()?.modified()?;
            let mut byte = [0_u8]; file.seek(SeekFrom::Start(offset))?; file.read_exact(&mut byte)?;
            byte[0] ^= 1; file.seek(SeekFrom::Start(offset))?; file.write_all(&byte)?;
            if fault == "restored-mtime" {
                file.set_times(std::fs::FileTimes::new().set_modified(modified))?;
                assert_eq!(file.metadata()?.modified()?, modified);
            }
        }
        assert!(eager_local_open(&index, &cache).is_err(), "{fault} must fail closed");
        assert!(KeeperSnapshot::open(&index, DEFAULT_SCHEMA).is_err(), "strict {fault}");
    }
    Ok(())
}

#[test]
fn local_receipt_proof_damage_is_a_full_check_not_admission_or_index_failure() -> TestResult {
    let (_root, index, cache, _) = local_receipt_fixture()?;
    drop(eager_local_open(&index, &cache)?);
    let path = cache.join("read-open-receipts-v2");
    let good = std::fs::read(&path)?;
    for damage in [b"".to_vec(), b"foreign-format".to_vec(), good[..good.len()-1].to_vec(), {
        let mut flipped = good.clone(); flipped[60] ^= 1; flipped
    }, vec![0; (1 << 20) + 1]] {
        std::fs::write(&path, damage)?;
        let fresh = eager_local_open(&index, &cache)?;
        assert_eq!(fresh.segments()[0].authenticated_file_witness_hash_count(), 1);
        drop(fresh);
        let repaired = eager_local_open(&index, &cache)?;
        assert_eq!(repaired.segments()[0].authenticated_file_witness_hash_count(), 0);
        drop(repaired);
    }
    Ok(())
}

#[test]
fn local_receipts_do_not_bless_stale_lazy_section_checksums() -> TestResult {
    let (_root, index, cache, _) = local_receipt_fixture()?;
    let encoded = encoded_test_segment(0xea501, 10, 20, 1)?;
    let doclen = encoded.section_entries().iter().find(|entry| entry.kind == SectionKind::DOCLEN).expect("doclen");
    let mut bytes = encoded.as_bytes().to_vec(); bytes[usize::try_from(doclen.offset)?] ^= 0x80;
    let file_xxh3 = reseal_test_segment_file_witness(&mut bytes)?;
    let path = index.join(canonical_segment_name(0xea501)); std::fs::write(path, &bytes)?;
    let mut record = manifest_segment(&encoded, 1); record.file_xxh3 = file_xxh3;
    write_manifest(&index.join("MANIFEST"), &durable_test_manifest(1, vec![record]))?;
    std::thread::sleep(Duration::from_millis(1100));
    // The prefix legitimately matches the MANIFEST, but DOCLEN's checksum is
    // stale. Caching only the prefix MUST NOT suppress that independent error.
    for expected_hashes in [1, 0] {
        let snapshot = eager_local_open(&index, &cache)?;
        assert_eq!(snapshot.segments()[0].authenticated_file_witness_hash_count(), expected_hashes);
        assert!(matches!(snapshot.segments()[0].section(SectionKind::DOCLEN), Err(QuillError::IndexCorrupted { .. })));
        drop(snapshot);
    }
    Ok(())
}

#[test]
fn local_receipts_never_write_inside_the_index() -> TestResult {
    use std::os::unix::fs::PermissionsExt;
    let (_root, index, _cache, _) = local_receipt_fixture()?;
    std::fs::set_permissions(&index, std::fs::Permissions::from_mode(0o700))?;
    let snapshot = eager_local_open(&index, &index)?;
    assert_eq!(snapshot.segments()[0].authenticated_file_witness_hash_count(), 1);
    assert!(!index.join("read-open-receipts-v2").exists());
    drop(snapshot);
    let nested = index.join("receipt-cache");
    std::fs::create_dir(&nested)?;
    std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o700))?;
    let snapshot = eager_local_open(&index, &nested)?;
    assert_eq!(snapshot.segments()[0].authenticated_file_witness_hash_count(), 1);
    assert!(!nested.join("read-open-receipts-v2").exists());
    Ok(())
}
