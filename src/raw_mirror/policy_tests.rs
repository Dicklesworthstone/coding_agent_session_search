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

fn run_usage_peer(test_name: &str, root_variable: &str, fixture: &Path) -> Result<()> {
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command
        .args(["--exact", test_name, "--nocapture"])
        .env(root_variable, fixture)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    crate::sources::configure_child_process_group(&mut command);
    let output = crate::sources::wait_for_child_output_with_limit(
        command.spawn()?, Duration::from_secs(30), Some(1024 * 1024),
    )?.ok_or_else(|| anyhow!("peer operation exceeded its deadline"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "peer operation failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr));
    assert!(stdout.contains("1 passed; 0 failed"), "peer test did not run: {stdout}");
    Ok(())
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

/// Indexing captured every session after walking the whole mirror, which made
/// a full index quadratic in mirror files. Captures far from the cap share one
/// walk; the shared total still refuses past the cap.
#[test]
fn captures_far_from_the_cap_share_one_inventory_walk() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let cap = 1024 * 1024 * 1024;
    for session in 0..40 {
        let source = temp.path().join(format!("session-{session}.jsonl"));
        fs::write(&source, transcript(session, 2 * 1024)).unwrap();
        capture(input(&data, &source), CHUNK as u64, CHUNK, policy(cap)).unwrap();
    }
    let root = raw_mirror_root(&data);
    assert_eq!(measured_usage_walks(&root), 1, "one walk for 40 new sessions");
    assert_eq!(storage_summary(&data).manifest_count, 40);

    // Admission close to the cap measures again, and refuses on the measured
    // total rather than the shared one.
    let used = measured_usage(&root).unwrap();
    let source = temp.path().join("one-more.jsonl");
    fs::write(&source, transcript(99, 16 * 1024)).unwrap();
    let error = capture(input(&data, &source), CHUNK as u64, CHUNK, policy(used + 8192)).unwrap_err();
    let paused = error.downcast_ref::<RawMirrorCapturePaused>().expect("paused");
    assert_eq!(paused.used_bytes, used);
    assert_eq!(measured_usage_walks(&root), 2);

    // A mirror already far over the cap refuses on the walked bytes without
    // walking again: every capture of a paused archive used to walk it.
    let error = capture(input(&data, &source), CHUNK as u64, CHUNK, policy(used / 4)).unwrap_err();
    assert_eq!(error.downcast_ref::<RawMirrorCapturePaused>().expect("paused").used_bytes, used);
    assert_eq!(measured_usage_walks(&root), 2);
}

/// A separate writer must invalidate a long-lived process's low inventory.
/// Exercise real capture/publication in both processes, not a synthetic total.
#[test]
fn a_peer_capture_refreshes_usage_before_admitting_another_snapshot() -> Result<()> {
    const PEER_ROOT: &str = "CASS_TEST_RAW_MIRROR_USAGE_PEER_ROOT";
    let peer_root = match dotenvy::var(PEER_ROOT) {
        Ok(path) => Some(PathBuf::from(path)),
        Err(dotenvy::Error::EnvVar(std::env::VarError::NotPresent)) => None,
        Err(error) => return Err(error.into()),
    };
    if let Some(fixture) = peer_root {
        let data = fixture.join("data");
        let source = fixture.join("peer.jsonl");
        capture(input(&data, &source), CHUNK as u64, CHUNK, policy(1024 * 1024 * 1024))?;
        assert_eq!(storage_summary(&data).manifest_count, 2);
        return Ok(());
    }

    let temp = tempfile::tempdir()?;
    let data = temp.path().join("data");
    let source = temp.path().join("original.jsonl");
    let original = transcript(0, 1024);
    fs::write(&source, &original)?;
    let first = capture(input(&data, &source), CHUNK as u64, CHUNK, policy(96 * 1024))?;
    let root = raw_mirror_root(&data);
    assert_eq!(measured_usage_walks(&root), 1);
    fs::write(temp.path().join("peer.jsonl"), transcript(1, 192 * 1024))?;

    run_usage_peer(
        "raw_mirror::store::policy::tests::a_peer_capture_refreshes_usage_before_admitting_another_snapshot",
        PEER_ROOT, temp.path(),
    )?;

    let after_peer = measured_usage(&root)?;
    assert!(after_peer > 96 * 1024, "peer must exceed the parent's admission cap");
    let next = temp.path().join("next.jsonl");
    let next_bytes = transcript(2, 1024);
    fs::write(&next, &next_bytes)?;
    let error = capture(input(&data, &next), CHUNK as u64, CHUNK, policy(96 * 1024))
        .expect_err("a peer's bytes must count before another snapshot is admitted");
    let paused = error.downcast_ref::<RawMirrorCapturePaused>()
        .ok_or_else(|| anyhow!("expected budget refusal, got {error:#}"))?;
    assert_eq!(paused.used_bytes, after_peer);
    assert_eq!(storage_summary(&data).manifest_count, 2);
    assert_eq!(contents(&data, &first), original);
    assert_eq!(measured_usage_walks(&root), 2, "handoff requires a fresh inventory");
    assert_staging_empty(&data);

    let admitted = capture(input(&data, &next), CHUNK as u64, CHUNK, policy(1024 * 1024 * 1024))?;
    assert_eq!(contents(&data, &admitted), next_bytes);
    assert_eq!(storage_summary(&data).manifest_count, 3);
    assert_eq!(measured_usage_walks(&root), 2, "same-process capture reuses the new total");
    Ok(())
}

#[test]
fn a_path_alias_cannot_reuse_another_inventory_of_the_same_store() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path().join("data");
    let source = temp.path().join("first.jsonl");
    let original = transcript(0, 1024);
    fs::write(&source, &original)?;
    let first = capture(input(&data, &source), CHUNK as u64, CHUNK, policy(96 * 1024))?;
    let peer = temp.path().join("alias.jsonl");
    fs::write(&peer, transcript(1, 192 * 1024))?;
    let alias = data.join("..").join("data");
    capture(input(&alias, &peer), CHUNK as u64, CHUNK, policy(1024 * 1024 * 1024))?;
    let used = measured_usage(&raw_mirror_root(&data))?;
    assert!(used > 96 * 1024);
    let next = temp.path().join("next.jsonl");
    fs::write(&next, transcript(2, 1024))?;
    let error = capture(input(&data, &next), CHUNK as u64, CHUNK, policy(96 * 1024))
        .expect_err("an alias must not hide another writer's bytes");
    let paused = error.downcast_ref::<RawMirrorCapturePaused>()
        .ok_or_else(|| anyhow!("expected budget refusal: {error:#}"))?;
    assert_eq!(paused.used_bytes, used);
    assert_eq!(storage_summary(&data).manifest_count, 2);
    assert_eq!(contents(&data, &first), original);
    Ok(())
}

#[test]
fn a_peer_prune_refreshes_a_cached_refusal_before_the_next_capture() -> Result<()> {
    const PEER_ROOT: &str = "CASS_TEST_RAW_MIRROR_PRUNE_PEER_ROOT";
    let peer_root = match dotenvy::var(PEER_ROOT) {
        Ok(path) => Some(PathBuf::from(path)),
        Err(dotenvy::Error::EnvVar(std::env::VarError::NotPresent)) => None,
        Err(error) => return Err(error.into()),
    };
    if let Some(fixture) = peer_root {
        let report = prune(&fixture.join("data"), RawMirrorPruneOptions {
            older_than_ms: Some(0), max_size_bytes: None, providers: Vec::new(),
            source_path: None, keep_tags: Vec::new(), keep_latest: None,
            safety_hold_down_ms: 0, apply: true,
        })?;
        assert_eq!(report.applied_manifest_count, 1);
        return Ok(());
    }
    let temp = tempfile::tempdir()?;
    let data = temp.path().join("data");
    let source = temp.path().join("first.jsonl");
    fs::write(&source, transcript(0, 192 * 1024))?;
    capture(input(&data, &source), CHUNK as u64, CHUNK, policy(1024 * 1024 * 1024))?;
    let next = temp.path().join("next.jsonl");
    let next_bytes = transcript(1, 1024);
    fs::write(&next, &next_bytes)?;
    let error = capture(input(&data, &next), CHUNK as u64, CHUNK, policy(96 * 1024))
        .expect_err("prime an actual over-budget inventory");
    assert!(error.is::<RawMirrorCapturePaused>());
    run_usage_peer(
        "raw_mirror::store::policy::tests::a_peer_prune_refreshes_a_cached_refusal_before_the_next_capture",
        PEER_ROOT, temp.path(),
    )?;
    let root = raw_mirror_root(&data);
    assert!(measured_usage(&root)? < 96 * 1024);
    assert_eq!(storage_summary(&data).manifest_count, 0);
    let captured = capture(input(&data, &next), CHUNK as u64, CHUNK, policy(96 * 1024))?;
    assert_eq!(contents(&data, &captured), next_bytes);
    assert_eq!(storage_summary(&data).manifest_count, 1);
    assert!(measured_usage(&root)? <= 96 * 1024);
    Ok(())
}

#[test]
fn direct_manifest_link_growth_counts_before_the_next_admission() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path().join("data");
    let source = temp.path().join("first.jsonl");
    let original = transcript(0, 1024);
    fs::write(&source, &original)?;
    let first = capture(input(&data, &source), CHUNK as u64, CHUNK, policy(96 * 1024))?;
    let links = (1..=2048).map(|id| RawMirrorDbLink {
        conversation_id: Some(id), message_count: Some(1), source_path: None,
        started_at_ms: None,
    }).collect::<Vec<_>>();
    merge_manifest_db_links(&data, &first.manifest_relative_path, &links)?;
    let root = raw_mirror_root(&data);
    let used = measured_usage(&root)?;
    assert!(used > 96 * 1024, "the real metadata update must exceed the cap");
    let next = temp.path().join("next.jsonl");
    fs::write(&next, transcript(1, 1024))?;
    let error = capture(input(&data, &next), CHUNK as u64, CHUNK, policy(96 * 1024))
        .expect_err("metadata growth must count against capture admission");
    let paused = error.downcast_ref::<RawMirrorCapturePaused>()
        .ok_or_else(|| anyhow!("expected budget refusal: {error:#}"))?;
    assert_eq!(paused.used_bytes, used);
    assert_eq!(measured_usage_walks(&root), 2);
    assert_eq!(storage_summary(&data).manifest_count, 1);
    assert_eq!(contents(&data, &first), original);
    Ok(())
}

#[test]
fn malformed_or_exhausted_usage_generations_preserve_existing_snapshots() -> Result<()> {
    let mut exhausted = b"CASSRMU1".to_vec();
    exhausted.extend_from_slice(&u64::MAX.to_le_bytes());
    for invalid in [b"bad".to_vec(), b"WRONGTAG00000000".to_vec(), exhausted] {
        let temp = tempfile::tempdir()?;
        let data = temp.path().join("data");
        let source = temp.path().join("first.jsonl");
        let original = transcript(0, 1024);
        fs::write(&source, &original)?;
        let first = capture(input(&data, &source), CHUNK as u64, CHUNK, policy(1024 * 1024))?;
        let path = raw_mirror_root(&data).join(USAGE_GENERATION_FILE);
        fs::write(&path, &invalid)?;
        let error = capture(input(&data, &source), CHUNK as u64, CHUNK, policy(1024 * 1024))
            .expect_err("an invalid coordination record cannot authorize mutation");
        assert!(error.to_string().contains("raw-mirror usage generation"), "{error:#}");
        assert_eq!(fs::read(&path)?, invalid, "invalid evidence must stay intact");
        assert_eq!(contents(&data, &first), original);
        assert_eq!(storage_summary(&data).manifest_count, 1);
    }
    Ok(())
}

#[test]
#[cfg(unix)]
fn a_symlinked_usage_generation_is_refused_and_preserves_its_target() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path().join("data");
    let source = temp.path().join("first.jsonl");
    let original = transcript(0, 1024);
    fs::write(&source, &original)?;
    let first = capture(input(&data, &source), CHUNK as u64, CHUNK, policy(1024 * 1024))?;
    let root = raw_mirror_root(&data);
    let generation = root.join(USAGE_GENERATION_FILE);
    fs::rename(&generation, root.join("generation-retained"))?;
    let outside = temp.path().join("outside-evidence");
    let mut bytes = b"CASSRMU1".to_vec();
    bytes.extend_from_slice(&41_u64.to_le_bytes());
    fs::write(&outside, &bytes)?;
    std::os::unix::fs::symlink(&outside, &generation)?;
    let error = capture(input(&data, &source), CHUNK as u64, CHUNK, policy(1024 * 1024))
        .expect_err("coordination cannot follow a symlink");
    assert!(error.to_string().contains("raw-mirror usage generation"), "{error:#}");
    assert_eq!(fs::read(&outside)?, bytes);
    assert_eq!(contents(&data, &first), original);
    assert_eq!(storage_summary(&data).manifest_count, 1);
    Ok(())
}

#[test]
#[cfg(unix)]
fn replacing_a_hardlinked_usage_generation_preserves_the_other_file() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path().join("data");
    let source = temp.path().join("first.jsonl");
    let original = transcript(0, 1024);
    fs::write(&source, &original)?;
    let first = capture(input(&data, &source), CHUNK as u64, CHUNK, policy(1024 * 1024))?;
    let root = raw_mirror_root(&data);
    let generation = root.join(USAGE_GENERATION_FILE);
    fs::rename(&generation, root.join("generation-retained"))?;
    let outside = temp.path().join("outside-evidence");
    let mut bytes = b"CASSRMU1".to_vec();
    bytes.extend_from_slice(&41_u64.to_le_bytes());
    fs::write(&outside, &bytes)?;
    fs::hard_link(&outside, &generation)?;
    fs::write(&source, transcript(1, 2048))?;
    capture(input(&data, &source), CHUNK as u64, CHUNK, policy(1024 * 1024))?;
    assert_eq!(fs::read(&outside)?, bytes, "publication must never write through a hard link");
    assert_ne!(fs::read(&generation)?, bytes);
    assert_eq!(contents(&data, &first), original);
    assert_eq!(storage_summary(&data).manifest_count, 2);
    Ok(())
}

/// A prune in this process deletes files the shared total still counts, so the
/// next admission must walk again instead of refusing on the stale total.
#[test]
fn a_prune_drops_the_shared_inventory() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let source = temp.path().join("session.jsonl");
    fs::write(&source, transcript(0, 4 * 1024)).unwrap();
    capture(input(&data, &source), CHUNK as u64, CHUNK, policy(1024 * 1024 * 1024)).unwrap();
    let root = raw_mirror_root(&data);
    assert_eq!(measured_usage_walks(&root), 1);
    let report = prune(&data, RawMirrorPruneOptions {
        older_than_ms: Some(0), max_size_bytes: None, providers: Vec::new(), source_path: None,
        keep_tags: Vec::new(), keep_latest: None, safety_hold_down_ms: 0, apply: true,
    }).unwrap();
    assert_eq!(report.applied_manifest_count, 1, "{report:?}");
    assert_eq!(measured_usage_walks(&root), 0, "the prune dropped the shared total");
    fs::write(&source, transcript(1, 4 * 1024)).unwrap();
    capture(input(&data, &source), CHUNK as u64, CHUNK, policy(1024 * 1024 * 1024)).unwrap();
    assert_eq!(measured_usage_walks(&root), 1, "the next admission walked again");
}
