use super::*;

const CHUNK: usize = 256 * 1024;
fn policy(max_bytes: u64) -> RawMirrorRetentionPolicy {
    RawMirrorRetentionPolicy { max_bytes, min_free_bytes: 0 }
}
fn input<'a>(data: &'a Path, source: &'a Path) -> RawMirrorCaptureInput<'a> {
    RawMirrorCaptureInput { data_dir: data, provider: "codex", source_id: "local",
        origin_kind: "local", origin_host: None, source_path: source, db_links: &[] }
}
fn transcript(round: usize, bytes: usize) -> Vec<u8> {
    let mut result = Vec::new();
    let mut line = 0;
    while result.len() < bytes {
        result.extend(format!("{{\"type\":\"response_item\",\"round\":{round},\"line\":{line},\"text\":\"append regression\"}}\n").as_bytes());
        line += 1;
    }
    result
}
fn contents(data: &Path, record: &RawMirrorCaptureRecord) -> Vec<u8> {
    let root = raw_mirror_root(data);
    let manifest = read_validated_raw_mirror_manifest(&root, &record.manifest_id).unwrap();
    for reference in raw_mirror_manifest_blob_references(&manifest).unwrap() {
        verify_existing_blob_reference(&root, &reference).unwrap();
    }
    match manifest.content_storage {
        Some(storage) => storage.chunks.iter().flat_map(|chunk|
            fs::read(root.join(&chunk.blob_relative_path)).unwrap()).collect(),
        None => fs::read(root.join(manifest.blob_relative_path)).unwrap(),
    }
}
fn assert_staging_empty(data: &Path) {
    let tmp = raw_mirror_root(data).join("tmp");
    if tmp.exists() { assert_eq!(fs::read_dir(tmp).unwrap().count(), 0); }
}

#[test]
fn frequent_appends_reuse_chunks_and_preserve_every_published_snapshot() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let source = temp.path().join("session.jsonl");
    let mut expected = transcript(0, 400 * 1024);
    fs::write(&source, &expected).unwrap();
    let cap = 16 * 1024 * 1024;
    let first = capture(input(&data, &source), CHUNK as u64, CHUNK, policy(cap)).unwrap();
    let original = expected.clone();
    for round in 1..=40 {
        let append = transcript(round, 16 * 1024);
        OpenOptions::new().append(true).open(&source).unwrap().write_all(&append).unwrap();
        expected.extend(append);
        let record = capture(input(&data, &source), CHUNK as u64, CHUNK, policy(cap)).unwrap();
        assert_eq!(contents(&data, &record), expected);
        assert!(measured_usage(&raw_mirror_root(&data)).unwrap() <= cap);
        assert_staging_empty(&data);
    }
    assert_eq!(contents(&data, &first), original);
    // Whole-file capture would add tens of MB for this workload.
    assert!(measured_usage(&raw_mirror_root(&data)).unwrap() < 10 * 1024 * 1024);
}

#[test]
fn partial_staging_is_removed_when_the_next_chunk_cannot_fit() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let source = temp.path().join("session.jsonl");
    fs::write(&source, transcript(0, 256 * 1024)).unwrap();
    let error = capture(input(&data, &source), 1, 32 * 1024, policy(100 * 1024)).unwrap_err();
    assert!(error.is::<RawMirrorCapturePaused>(), "{error:#}");
    assert_staging_empty(&data);
    assert_eq!(storage_summary(&data).unique_blob_count, 0);
    assert_eq!(storage_summary(&data).manifest_count, 0);
}

#[test]
fn an_unchanged_capture_at_the_cap_is_not_an_outage() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let source = temp.path().join("session.jsonl");
    fs::write(&source, transcript(0, 32 * 1024)).unwrap();
    let first = capture(input(&data, &source), CHUNK as u64, CHUNK, policy(1024 * 1024)).unwrap();
    let used = measured_usage(&raw_mirror_root(&data)).unwrap();
    for _ in 0..10 {
        let again = capture(input(&data, &source), CHUNK as u64, CHUNK, policy(used)).unwrap();
        assert!(again.already_present);
        assert_eq!(again.manifest_id, first.manifest_id);
    }
    assert_eq!(measured_usage(&raw_mirror_root(&data)).unwrap(), used);
}

#[test]
fn refused_append_leaves_the_last_recovery_copy_intact() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let source = temp.path().join("session.jsonl");
    let original = transcript(0, 32 * 1024);
    fs::write(&source, &original).unwrap();
    let first = capture(input(&data, &source), CHUNK as u64, CHUNK, policy(1024 * 1024)).unwrap();
    let used = measured_usage(&raw_mirror_root(&data)).unwrap();
    OpenOptions::new().append(true).open(&source).unwrap().write_all(&transcript(1, 16 * 1024)).unwrap();
    let error = capture(input(&data, &source), CHUNK as u64, CHUNK, policy(used + 8192)).unwrap_err();
    assert!(error.is::<RawMirrorCapturePaused>());
    assert_eq!(contents(&data, &first), original);
    assert_eq!(measured_usage(&raw_mirror_root(&data)).unwrap(), used);
    assert_staging_empty(&data);
}

#[test]
fn growing_source_reads_only_the_admitted_length_and_publishes_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let source = temp.path().join("session.jsonl");
    fs::write(&source, b"initial transcript\n").unwrap();
    let metadata = fs::metadata(&source).unwrap();
    OpenOptions::new().append(true).open(&source).unwrap().write_all(&transcript(1, 1024 * 1024)).unwrap();
    let root = ensure_raw_mirror_root(&data).unwrap();
    let mut staging = Staging::new(&root).unwrap();
    let mut budget = Budget { root: &root, policy: policy(2 * 1024 * 1024), used: 0, inventoried: true };
    let result = prepare_bounded(&root, &input(&data, &source), &metadata, 1, CHUNK, &mut staging, &mut budget);
    assert!(result.is_err());
    let staged: u64 = staging.files.iter().map(|path| fs::metadata(path).unwrap().len()).sum();
    assert_eq!(staged, metadata.len(), "do not chase the new megabyte to EOF");
    drop(staging);
    assert_staging_empty(&data);
    assert_eq!(storage_summary(&data).manifest_count, 0);
}

#[test]
fn disk_pressure_pauses_capture_and_recovery_resumes_it() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let source = temp.path().join("session.jsonl");
    fs::write(&source, transcript(0, 1024)).unwrap();
    let pressured = RawMirrorRetentionPolicy { max_bytes: u64::MAX, min_free_bytes: u64::MAX };
    let error = capture(input(&data, &source), 1, CHUNK, pressured).unwrap_err();
    let diagnostic = error.downcast_ref::<RawMirrorCapturePaused>().unwrap();
    assert!(diagnostic.available_bytes < diagnostic.min_free_bytes);
    assert!(diagnostic.to_string().contains("cass mirror prune"));
    assert_staging_empty(&data);
    let recovered = capture(input(&data, &source), 1, CHUNK, policy(1024 * 1024)).unwrap();
    assert_eq!(contents(&data, &recovered), fs::read(&source).unwrap());
}

#[test]
fn concurrent_captures_share_one_disk_budget() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let mut handles = Vec::new();
    for round in 0..2 {
        let source = temp.path().join(format!("session-{round}.jsonl"));
        fs::write(&source, transcript(round, 64 * 1024)).unwrap();
        let data = data.clone();
        handles.push(std::thread::spawn(move ||
            capture(input(&data, &source), CHUNK as u64, CHUNK, policy(110 * 1024))));
    }
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert!(results.iter().filter_map(|r| r.as_ref().err()).all(|e| e.is::<RawMirrorCapturePaused>()));
    assert!(measured_usage(&raw_mirror_root(&data)).unwrap() <= 110 * 1024);
    assert_staging_empty(&data);
}

#[test]
fn whole_blob_and_chunked_captures_remain_mutually_readable() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let source = temp.path().join("session.jsonl");
    let original = transcript(0, 128 * 1024);
    fs::write(&source, &original).unwrap();
    let legacy = super::super::capture_source_file_with_chunk_policy(input(&data, &source), 8 * 1024 * 1024, 4 * 1024 * 1024).unwrap();
    let bounded = capture(input(&data, &source), 1, 32 * 1024, policy(1024 * 1024)).unwrap();
    assert_eq!(legacy.storage_kind, "whole_blob_v1");
    assert_eq!(bounded.storage_kind, "fixed_chunks_v1");
    assert_eq!(contents(&data, &legacy), original);
    assert_eq!(contents(&data, &bounded), original);
}

#[cfg(unix)]
#[test]
fn persisted_cache_is_still_usable_without_a_source_copy() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let source = temp.path().join("session.jsonl");
    fs::write(&source, transcript(0, 512 * 1024)).unwrap();
    let first = capture(input(&data, &source), CHUNK as u64, CHUNK, policy(4 * 1024 * 1024)).unwrap();
    let key = raw_mirror_blob_cache_key(&input(&data, &source), &fs::metadata(&source).unwrap(), CHUNK as u64, CHUNK);
    BLOB_CAPTURE_CACHE.get().unwrap().lock().unwrap().remove(&raw_mirror_blob_source_key(&key));
    let cached = cached_record(&raw_mirror_root(&data), &key).expect("persisted cache hit");
    assert_eq!(cached.source_content_blake3, first.source_content_blake3);
}

#[test]
fn budget_math_fails_closed_on_overflow_and_low_space() {
    let temp = tempfile::tempdir().unwrap();
    let mut budget = Budget { root: temp.path(), policy: policy(u64::MAX), used: u64::MAX - 4, inventoried: true };
    assert!(budget.reserve_with_available(8, u64::MAX).is_err());
    budget.used = 0;
    budget.policy.min_free_bytes = 100;
    assert!(budget.reserve_with_available(1, 100).is_err());
    assert!(budget.reserve_with_available(1, 4197).is_ok());
}

#[cfg(unix)]
#[test]
fn admission_refuses_symlinked_inventory_without_following_it() {
    let temp = tempfile::tempdir().unwrap();
    let outside = temp.path().join("outside");
    fs::write(&outside, b"private evidence").unwrap();
    let data = temp.path().join("data");
    let root = ensure_raw_mirror_root(&data).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("unexpected")).unwrap();
    assert!(measured_usage(&root).is_err());
    assert_eq!(fs::read(outside).unwrap(), b"private evidence");
}
