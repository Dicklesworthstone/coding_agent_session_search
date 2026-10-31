use super::*;

struct Fixture {
    _temp: tempfile::TempDir,
    data: PathBuf,
    source: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        Self { data: temp.path().join("data"), source: temp.path().join("session.jsonl"), _temp: temp }
    }
    fn record(&self, contents: &[u8], time: i64) -> RawMirrorCaptureRecord {
        self.origin_record(contents, time, "local", &[])
    }
    fn origin_record(&self, contents: &[u8], time: i64, source_id: &str, links: &[RawMirrorDbLink]) -> RawMirrorCaptureRecord {
        fs::write(&self.source, contents).unwrap();
        let record = capture(RawMirrorCaptureInput {
            data_dir: &self.data, provider: "codex", source_id, origin_kind: "local",
            origin_host: None, source_path: &self.source, db_links: links,
        }, 256 * 1024, 256 * 1024, RawMirrorRetentionPolicy { max_bytes: 32 * 1024 * 1024, min_free_bytes: 0 }).unwrap();
        self.edit(&record, |manifest| manifest.captured_at_ms = time);
        record
    }
    fn edit(&self, record: &RawMirrorCaptureRecord, edit: impl FnOnce(&mut RawMirrorManifestFile)) {
        let root = raw_mirror_root(&self.data);
        let mut manifest = read_validated_raw_mirror_manifest(&root, &record.manifest_id).unwrap();
        edit(&mut manifest);
        manifest.manifest_blake3 = Some(raw_mirror_manifest_blake3(&manifest));
        fs::write(root.join(&record.manifest_relative_path), serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
    }
    fn policy(&self) -> RawMirrorAutomaticRetention {
        RawMirrorAutomaticRetention {
            capture: RawMirrorRetentionPolicy { max_bytes: 2 * 1024 * 1024, min_free_bytes: 0 },
            keep_latest: 2, safety_hold_down_ms: 0, keep_tags: Vec::new(),
        }
    }
    fn exists(&self, record: &RawMirrorCaptureRecord) -> bool {
        raw_mirror_root(&self.data).join(&record.manifest_relative_path).exists()
    }
    fn bytes(&self, record: &RawMirrorCaptureRecord) -> Vec<u8> {
        let root = raw_mirror_root(&self.data);
        let manifest = read_validated_raw_mirror_manifest(&root, &record.manifest_id).unwrap();
        verify_capture(&root, &manifest, &mut HashSet::new()).unwrap();
        content_chunks(&manifest).iter().flat_map(|r| fs::read(root.join(&r.blob_relative_path)).unwrap()).collect()
    }
}
fn transcript(round: usize, size: usize) -> Vec<u8> {
    let mut result = Vec::new();
    let mut line = 0;
    while result.len() < size {
        result.extend(format!("{{\"round\":{round},\"line\":{line},\"text\":\"retention regression\"}}\n").as_bytes());
        line += 1;
    }
    result
}

#[test]
fn eighty_index_intervals_have_bounded_growth_and_readable_latest_snapshots() {
    let fixture = Fixture::new();
    let mut bytes = transcript(0, 400 * 1024);
    let mut records = Vec::new();
    let mut retired = 0;
    for round in 0..80 {
        let maintenance = maintain(&fixture.data, &fixture.policy(), 10_000).unwrap();
        retired += maintenance.retired_manifests;
        // A real pre-index pass releases both leases before capture starts.
        bytes.extend(transcript(round, 8 * 1024));
        records.push(fixture.record(&bytes, round as i64 + 1));
        assert_eq!(fixture.bytes(records.last().unwrap()), bytes);
        assert!(measured_usage(&raw_mirror_root(&fixture.data)).unwrap() < 3 * 1024 * 1024);
    }
    let final_report = maintain(&fixture.data, &fixture.policy(), 10_000).unwrap();
    assert_eq!(retired + final_report.retired_manifests, 78);
    assert_eq!(collect_prune_manifests(&raw_mirror_root(&fixture.data)).unwrap().len(), 2);
    assert_eq!(fixture.bytes(records.last().unwrap()), bytes);
    assert!(fixture.exists(&records[78]));
    assert!(!fixture.exists(&records[0]));
}

#[test]
fn hold_down_blocks_retirement_even_when_the_cap_is_exceeded() {
    let fixture = Fixture::new();
    let records = [fixture.record(b"one\n", 100), fixture.record(b"one\ntwo\n", 200), fixture.record(b"one\ntwo\nthree\n", 300)];
    let mut policy = fixture.policy();
    policy.capture.max_bytes = 1;
    policy.safety_hold_down_ms = 1000;
    let held = maintain(&fixture.data, &policy, 500).unwrap();
    assert_eq!(held.retired_manifests, 0);
    assert!(held.capture_paused);
    assert!(records.iter().all(|record| fixture.exists(record)));
    assert_eq!(maintain(&fixture.data, &policy, 2000).unwrap().retired_manifests, 1);
    assert!(fixture.exists(&records[1]) && fixture.exists(&records[2]));
}

#[test]
fn truncations_rewrites_and_missing_upstream_sources_keep_unique_evidence() {
    let fixture = Fixture::new();
    let first = fixture.record(b"original evidence\n", 1);
    let rewritten = fixture.record(b"rewritten evidence\n", 2);
    let truncated = fixture.record(b"short\n", 3);
    let latest = fixture.record(b"short\nappended\n", 4);
    fs::rename(&fixture.source, fixture.source.with_extension("moved")).unwrap();
    let report = maintain(&fixture.data, &fixture.policy(), 10_000).unwrap();
    assert_eq!(report.retired_manifests, 0);
    for record in [&first, &rewritten, &truncated, &latest] { assert!(fixture.exists(record)); }
    assert_eq!(fixture.bytes(&first), b"original evidence\n");
}

#[test]
fn identical_paths_from_different_origins_are_not_successors() {
    let fixture = Fixture::new();
    let local = fixture.origin_record(b"shared prefix\n", 1, "local", &[]);
    fixture.origin_record(b"shared prefix\nremote addition\n", 2, "remote-a", &[]);
    fixture.origin_record(b"shared prefix\nother addition\n", 3, "remote-b", &[]);
    let mut policy = fixture.policy();
    policy.keep_latest = 1;
    assert_eq!(maintain(&fixture.data, &policy, 1000).unwrap().retired_manifests, 0);
    assert!(fixture.exists(&local));
}

#[test]
fn corruption_of_a_retained_successor_aborts_before_any_delete_or_audit() {
    let fixture = Fixture::new();
    let old = fixture.record(b"old\n", 1);
    fixture.record(b"old\nnew\n", 2);
    let latest = fixture.record(b"old\nnew\nlatest\n", 3);
    let root = raw_mirror_root(&fixture.data);
    fs::write(root.join(&latest.blob_relative_path), b"corrupt replacement!").unwrap();
    assert!(maintain(&fixture.data, &fixture.policy(), 1000).is_err());
    assert!(fixture.exists(&old));
    assert!(fixture.exists(&latest));
    assert_eq!(fixture.bytes(&old), b"old\n");
}

#[test]
fn recovery_verification_records_and_orphan_blobs_are_never_auto_deleted() {
    let fixture = Fixture::new();
    let evidence = fixture.record(b"preserve\n", 1);
    fixture.edit(&evidence, |m| { m.verification.status = "recovery_evidence".to_string(); });
    fixture.record(b"preserve\nnext\n", 2);
    fixture.record(b"preserve\nnext\nlatest\n", 3);
    let root = raw_mirror_root(&fixture.data);
    let orphan_bytes = b"crash recovery evidence";
    let hash = blake3::hash(orphan_bytes).to_hex().to_string();
    let path = root.join(raw_mirror_blob_relative_path(&hash).unwrap());
    ensure_private_dir_descendant(&root, path.parent().unwrap()).unwrap();
    fs::write(&path, orphan_bytes).unwrap();
    assert_eq!(maintain(&fixture.data, &fixture.policy(), 1000).unwrap().retired_manifests, 0);
    assert!(fixture.exists(&evidence));
    assert_eq!(fs::read(path).unwrap(), orphan_bytes);
}

#[test]
fn no_op_maintenance_is_idempotent_and_does_not_grow_the_audit() {
    let fixture = Fixture::new();
    fixture.record(b"one\n", 1);
    fixture.record(b"one\ntwo\n", 2);
    fixture.record(b"one\ntwo\nthree\n", 3);
    assert_eq!(maintain(&fixture.data, &fixture.policy(), 1000).unwrap().retired_manifests, 1);
    let used = measured_usage(&raw_mirror_root(&fixture.data)).unwrap();
    for _ in 0..10 { assert_eq!(maintain(&fixture.data, &fixture.policy(), 1000).unwrap().retired_manifests, 0); }
    assert_eq!(measured_usage(&raw_mirror_root(&fixture.data)).unwrap(), used);
}

#[test]
fn active_index_lease_prevents_automatic_retention() {
    let fixture = Fixture::new();
    fixture.record(b"one\n", 1);
    fixture.record(b"one\ntwo\n", 2);
    fixture.record(b"one\ntwo\nthree\n", 3);
    let lease = try_acquire_index_run_lock_for_prune(&fixture.data).unwrap();
    assert!(maintain(&fixture.data, &fixture.policy(), 1000).is_err());
    assert_eq!(collect_prune_manifests(&raw_mirror_root(&fixture.data)).unwrap().len(), 3);
    drop(lease);
    assert_eq!(maintain(&fixture.data, &fixture.policy(), 1000).unwrap().retired_manifests, 1);
}

#[test]
fn pre_index_reclamation_unpauses_a_mirror_that_was_over_its_budget() {
    let fixture = Fixture::new();
    let mut bytes = transcript(0, 400 * 1024);
    for round in 0..12 {
        bytes.extend(transcript(round, 4 * 1024));
        fixture.record(&bytes, round as i64 + 1);
    }
    let before = measured_usage(&raw_mirror_root(&fixture.data)).unwrap();
    let mut policy = fixture.policy();
    policy.capture.max_bytes = before / 2;
    let report = maintain(&fixture.data, &policy, 10_000).unwrap();
    assert!(report.used_bytes_after < policy.capture.max_bytes);
    assert!(!report.capture_paused);
    bytes.extend(transcript(12, 4 * 1024));
    fs::write(&fixture.source, &bytes).unwrap();
    let record = capture(RawMirrorCaptureInput {
        data_dir: &fixture.data, provider: "codex", source_id: "local", origin_kind: "local",
        origin_host: None, source_path: &fixture.source, db_links: &[],
    }, 256 * 1024, 256 * 1024, policy.capture).unwrap();
    assert_eq!(fixture.bytes(&record), bytes);
}

#[test]
fn retirement_preserves_links_and_uses_a_durable_intent_result_audit() {
    let fixture = Fixture::new();
    let link = RawMirrorDbLink { conversation_id: Some(42), message_count: Some(1),
        source_path: None, started_at_ms: Some(1) };
    let old = fixture.origin_record(b"one\n", 1, "local", std::slice::from_ref(&link));
    fixture.record(b"one\ntwo\n", 2);
    let newest = fixture.record(b"one\ntwo\nthree\n", 3);
    assert_eq!(maintain(&fixture.data, &fixture.policy(), 1000).unwrap().retired_manifests, 1);
    let root = raw_mirror_root(&fixture.data);
    let manifest = read_validated_raw_mirror_manifest(&root, &newest.manifest_id).unwrap();
    assert!(manifest.db_links.contains(&link));
    assert!(!fixture.exists(&old));
    let (audit_path, audit_file) = open_prune_audit_log(&root).unwrap();
    drop(audit_file);
    let audit = fs::read_to_string(audit_path).unwrap();
    assert!(audit.contains("intent") && audit.contains("result"));
    assert!(audit.contains(&old.manifest_id) && audit.contains(&newest.manifest_id));
    assert!(audit.find("intent").unwrap() < audit.find("result").unwrap());
}

#[test]
fn tagged_old_snapshot_stays_pinned_under_pressure() {
    let fixture = Fixture::new();
    let link = RawMirrorDbLink { conversation_id: Some(42), message_count: Some(1),
        source_path: None, started_at_ms: Some(1) };
    let old = fixture.origin_record(b"one\n", 1, "local", &[link]);
    fixture.record(b"one\ntwo\n", 2);
    fixture.record(b"one\ntwo\nthree\n", 3);
    let mut conn = crate::franken_sync::Connection::open(
        fixture.data.join("agent_search.db").to_string_lossy().as_ref()).unwrap();
    conn.execute("CREATE TABLE tags (id INTEGER PRIMARY KEY, name TEXT)").unwrap();
    conn.execute("CREATE TABLE conversation_tags (conversation_id INTEGER, tag_id INTEGER)").unwrap();
    conn.execute("INSERT INTO tags VALUES (1, 'pinned')").unwrap();
    conn.execute("INSERT INTO conversation_tags VALUES (42, 1)").unwrap();
    conn.close_in_place().unwrap();
    let mut policy = fixture.policy();
    policy.keep_tags = vec!["pinned".to_string()];
    policy.capture.max_bytes = 1;
    let result = maintain(&fixture.data, &policy, 1000).unwrap();
    assert_eq!(result.retired_manifests, 0);
    assert!(result.capture_paused);
    assert!(fixture.exists(&old));
}

#[test]
fn legacy_whole_blob_prefix_can_retire_without_a_format_migration() {
    let fixture = Fixture::new();
    let original = transcript(0, 192 * 1024);
    let old = fixture.record(&original, 1);
    let mut bytes = original.clone();
    bytes.extend(transcript(1, 100 * 1024));
    fixture.record(&bytes, 2);
    bytes.extend(transcript(2, 100 * 1024));
    let newest = fixture.record(&bytes, 3);
    assert_eq!(old.storage_kind, "whole_blob_v1");
    assert_eq!(newest.storage_kind, "fixed_chunks_v1");
    assert_eq!(maintain(&fixture.data, &fixture.policy(), 1000).unwrap().retired_manifests, 1);
    assert!(!fixture.exists(&old));
    assert!(fixture.bytes(&newest).starts_with(&original));
}

#[test]
fn gh538_retention_prefix_comparison_observes_cancellation_between_bounded_reads() {
    let fixture = Fixture::new();
    let original = transcript(0, 192 * 1024);
    let old = fixture.record(&original, 1);
    let mut appended = original.clone();
    appended.extend(transcript(1, 100 * 1024));
    let new = fixture.record(&appended, 2);
    let root = raw_mirror_root(&fixture.data);
    let old_manifest = read_validated_raw_mirror_manifest(&root, &old.manifest_id).unwrap();
    let new_manifest = read_validated_raw_mirror_manifest(&root, &new.manifest_id).unwrap();
    let old_bytes = fs::read(root.join(&old.manifest_relative_path)).unwrap();
    let new_bytes = fs::read(root.join(&new.manifest_relative_path)).unwrap();
    let completed = Arc::new(AtomicU64::new(0));
    let on_progress = completed.clone();
    let is_cancelled = completed.clone();
    {
        let _observer = observe_raw_mirror_io(
            Arc::new(move || {
                on_progress.fetch_add(1, Ordering::Relaxed);
            }),
            // Let real opens/seeks and the first bounded reads complete,
            // then interrupt the remaining prefix comparison.
            Arc::new(move || is_cancelled.load(Ordering::Relaxed) >= 6),
        );
        let error = preserved_prefix(&root, &old_manifest, &new_manifest)
            .expect_err("retention must not ignore a stop during a large prefix comparison");
        assert!(error.chain().any(|cause| {
            cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::Interrupted)
        }));
        assert!(completed.load(Ordering::Relaxed) > 0);
    }
    assert_eq!(
        fs::read(root.join(&old.manifest_relative_path)).unwrap(),
        old_bytes
    );
    assert_eq!(
        fs::read(root.join(&new.manifest_relative_path)).unwrap(),
        new_bytes
    );
    assert_eq!(fixture.bytes(&old), original);
    assert_eq!(fixture.bytes(&new), appended);
    assert!(preserved_prefix(&root, &old_manifest, &new_manifest).unwrap());
}

#[test]
fn gh538_cancelled_maintenance_releases_both_leases_without_retiring_evidence() {
    fn held_by_another_handle(file: &File) -> bool {
        match file.try_lock() {
            Ok(()) => {
                file.unlock().unwrap();
                false
            }
            Err(std::fs::TryLockError::WouldBlock) => true,
            Err(std::fs::TryLockError::Error(error)) => panic!("probe maintenance lease: {error}"),
        }
    }

    let fixture = Fixture::new();
    let old = fixture.record(b"one\n", 1);
    fixture.record(b"one\ntwo\n", 2);
    let latest = fixture.record(b"one\ntwo\nthree\n", 3);
    let root = raw_mirror_root(&fixture.data);
    // Pre-open independent handles; the observer does no path lookup while
    // deciding whether the actual maintenance owner holds both leases.
    drop(try_acquire_index_run_lock_for_prune(&fixture.data).unwrap());
    let index_probe = File::options()
        .read(true)
        .write(true)
        .open(fixture.data.join("index-run.lock"))
        .unwrap();
    let mutation_probe = File::options()
        .read(true)
        .write(true)
        .open(root.join(RAW_MIRROR_MUTATION_LOCK_FILE))
        .unwrap();
    let observed_both = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cancelled = observed_both.clone();
    {
        let _observer = observe_raw_mirror_io(
            Arc::new(|| {}),
            Arc::new(move || {
                let both =
                    held_by_another_handle(&index_probe) && held_by_another_handle(&mutation_probe);
                cancelled.fetch_or(both, Ordering::Relaxed);
                both
            }),
        );
        let error = maintain(&fixture.data, &fixture.policy(), 1000)
            .expect_err("stop must drain maintenance while both leases are held");
        assert!(error.chain().any(|cause| {
            cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::Interrupted)
        }));
    }
    assert!(observed_both.load(Ordering::Relaxed));
    let _index = try_acquire_index_run_lock_for_prune(&fixture.data).unwrap();
    let _mutation = acquire_raw_mirror_mutation_lock(&root).unwrap();
    assert_eq!(collect_prune_manifests(&root).unwrap().len(), 3);
    assert!(fixture.exists(&old));
    assert_eq!(fixture.bytes(&latest), b"one\ntwo\nthree\n");
    assert!(!root.join("pruned.jsonl").exists());
}
