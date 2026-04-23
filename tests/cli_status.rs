use assert_cmd::Command;
use coding_agent_search::search::tantivy::{SCHEMA_HASH, expected_index_dir};
use fs2::FileExt;
use serde_json::json;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::Duration;

fn write_quarantined_manifest(generation_dir: &Path) {
    fs::create_dir_all(generation_dir).expect("create generation dir");
    fs::write(
        generation_dir.join("lexical-generation-manifest.json"),
        serde_json::to_vec_pretty(&json!({
            "manifest_version": 1,
            "generation_id": "gen-quarantined",
            "attempt_id": "attempt-1",
            "created_at_ms": 1_733_000_000_000_i64,
            "updated_at_ms": 1_733_000_000_321_i64,
            "source_db_fingerprint": "fp-test",
            "conversation_count": 3,
            "message_count": 9,
            "indexed_doc_count": 9,
            "equivalence_manifest_fingerprint": null,
            "shard_plan": null,
            "build_budget": null,
            "shards": [{
                "shard_id": "shard-a",
                "shard_ordinal": 0,
                "state": "quarantined",
                "updated_at_ms": 1_733_000_000_222_i64,
                "indexed_doc_count": 9,
                "message_count": 9,
                "artifact_bytes": 512,
                "stable_hash": "stable-hash-a",
                "reclaimable": false,
                "pinned": false,
                "recovery_reason": null,
                "quarantine_reason": "validation_failed"
            }],
            "merge_debt": {
                "state": "none",
                "updated_at_ms": null,
                "pending_shard_count": 0,
                "pending_artifact_bytes": 0,
                "reason": null,
                "controller_reason": null
            },
            "build_state": "failed",
            "publish_state": "quarantined",
            "failure_history": []
        }))
        .expect("serialize manifest"),
    )
    .expect("write manifest");
}

fn seed_active_rebuild_runtime(data_dir: &Path) -> std::fs::File {
    let db_path = data_dir.join("agent_search.db");
    let index_path = expected_index_dir(data_dir);
    fs::create_dir_all(&index_path).expect("create index dir");
    fs::write(
        index_path.join(".lexical-rebuild-state.json"),
        serde_json::to_vec_pretty(&json!({
            "version": 2,
            "schema_hash": SCHEMA_HASH,
            "db": {
                "db_path": db_path.display().to_string(),
                "total_conversations": 10,
                "total_messages": 20,
                "storage_fingerprint": "seed:10"
            },
            "page_size": 1024,
            "committed_offset": 4,
            "committed_conversation_id": 4,
            "processed_conversations": 4,
            "indexed_docs": 20,
            "committed_meta_fingerprint": null,
            "pending": null,
            "completed": false,
            "updated_at_ms": 1_733_000_123_000_i64,
            "runtime": {
                "queue_depth": 3,
                "inflight_message_bytes": 65_536,
                "max_message_bytes_in_flight": 131_072,
                "pending_batch_conversations": 9,
                "pending_batch_message_bytes": 131_072,
                "page_prep_workers": 6,
                "active_page_prep_jobs": 2,
                "ordered_buffered_pages": 4,
                "budget_generation": 1,
                "producer_budget_wait_count": 2,
                "producer_budget_wait_ms": 17,
                "producer_handoff_wait_count": 1,
                "producer_handoff_wait_ms": 9,
                "host_loadavg_1m_milli": 7_250,
                "controller_mode": "pressure_limited",
                "controller_reason": "queue_depth_3_reached_pipeline_capacity_3",
                "staged_merge_workers_max": 3,
                "staged_merge_allowed_jobs": 1,
                "staged_merge_active_jobs": 1,
                "staged_merge_ready_artifacts": 5,
                "staged_merge_ready_groups": 1,
                "staged_merge_controller_reason": "page_prep_workers_saturated_6_of_6",
                "staged_shard_build_workers_max": 6,
                "staged_shard_build_allowed_jobs": 5,
                "staged_shard_build_active_jobs": 4,
                "staged_shard_build_pending_jobs": 2,
                "staged_shard_build_controller_reason": "reserving_1_slots_for_staged_merge_active_jobs_1_ready_groups_1",
                "updated_at_ms": 1_733_000_124_000_i64
            }
        }))
        .expect("serialize rebuild state"),
    )
    .expect("write rebuild state");

    let lock_path = data_dir.join("index-run.lock");
    let mut lock_file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&lock_path)
        .expect("open lock file");
    lock_file.lock_exclusive().expect("hold index lock");
    writeln!(
        lock_file,
        "pid={}\nstarted_at_ms={}\ndb_path={}\nmode=index",
        std::process::id(),
        1_733_000_111_000_i64,
        db_path.display()
    )
    .expect("write lock metadata");
    lock_file.flush().expect("flush lock metadata");
    lock_file
}

#[test]
fn status_json_surfaces_runtime_queue_and_byte_budget_headroom() {
    let test_home = tempfile::tempdir().expect("tempdir");
    let data_dir = test_home.path().join("cass-data");
    fs::create_dir_all(&data_dir).expect("create data dir");
    let _lock = seed_active_rebuild_runtime(&data_dir);

    let out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args([
            "status",
            "--data-dir",
            data_dir.to_str().expect("utf8"),
            "--json",
        ])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CASS_TANTIVY_REBUILD_PIPELINE_CHANNEL_SIZE", "5")
        .env("XDG_DATA_HOME", test_home.path())
        .env("HOME", test_home.path())
        .output()
        .expect("run cass status --json");
    assert!(
        out.status.success(),
        "cass status --json failed: {:?}",
        out.status
    );

    let stdout = String::from_utf8(out.stdout).expect("utf8");
    let payload: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let runtime = &payload["rebuild"]["pipeline"]["runtime"];

    assert_eq!(runtime["queue_depth"].as_u64(), Some(3));
    assert_eq!(runtime["queue_capacity"].as_u64(), Some(5));
    assert_eq!(runtime["queue_headroom"].as_u64(), Some(2));
    assert_eq!(runtime["inflight_message_bytes"].as_u64(), Some(65_536));
    assert_eq!(
        runtime["max_message_bytes_in_flight"].as_u64(),
        Some(131_072)
    );
    assert_eq!(
        runtime["inflight_message_bytes_headroom"].as_u64(),
        Some(65_536)
    );
}

#[test]
fn status_json_surfaces_quarantine_gc_summary() {
    let test_home = tempfile::tempdir().expect("tempdir");
    let data_dir = test_home.path().join("cass-data");
    let backups_dir = data_dir.join("backups");
    fs::create_dir_all(&backups_dir).expect("create backups dir");

    let failed_seed_root =
        backups_dir.join("agent_search.db.20260423T120000.12345.deadbeef.failed-baseline-seed.bak");
    fs::write(&failed_seed_root, b"seed-backup").expect("write failed seed bundle");
    fs::write(
        failed_seed_root.with_file_name(format!(
            "{}-wal",
            failed_seed_root
                .file_name()
                .and_then(|name| name.to_str())
                .expect("file name")
        )),
        b"seed-wal",
    )
    .expect("write failed seed wal");

    let index_path = expected_index_dir(&data_dir);
    fs::create_dir_all(&index_path).expect("create expected index dir");
    let retained_publish_dir = index_path
        .parent()
        .expect("index parent")
        .join(".lexical-publish-backups");
    fs::create_dir_all(&retained_publish_dir).expect("create retained publish dir");
    let older_backup = retained_publish_dir.join("prior-live-older");
    fs::create_dir_all(&older_backup).expect("create older retained backup");
    fs::write(older_backup.join("segment-a"), b"retained-live-segment-old")
        .expect("write older retained publish backup");
    std::thread::sleep(Duration::from_millis(20));
    let newer_backup = retained_publish_dir.join("prior-live-newer");
    fs::create_dir_all(&newer_backup).expect("create newer retained backup");
    fs::write(newer_backup.join("segment-b"), b"retained-live-segment-new")
        .expect("write newer retained publish backup");

    let generation_dir = index_path
        .parent()
        .expect("index parent")
        .join("generation-quarantined");
    write_quarantined_manifest(&generation_dir);
    fs::write(
        generation_dir.join("segment-a"),
        b"quarantined-generation-bytes",
    )
    .expect("write quarantined generation artifact");

    let out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args([
            "status",
            "--json",
            "--data-dir",
            data_dir.to_str().expect("utf8"),
        ])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CASS_LEXICAL_PUBLISH_BACKUP_RETENTION", "1")
        .env("XDG_DATA_HOME", test_home.path())
        .env("XDG_CONFIG_HOME", test_home.path())
        .env("HOME", test_home.path())
        .output()
        .expect("run cass status --json");
    assert!(
        out.status.success(),
        "cass status --json failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let payload: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    let summary = &payload["quarantine"]["summary"];

    assert_eq!(summary["gc_eligible_asset_count"].as_u64(), Some(1));
    assert!(
        summary["gc_eligible_bytes"].as_u64().unwrap_or(0) > 0,
        "one retained publish backup should fall outside the retention cap"
    );
    assert_eq!(summary["inspection_required_asset_count"].as_u64(), Some(3));
    assert!(
        summary["inspection_required_bytes"].as_u64().unwrap_or(0) > 0,
        "failed seed bundles and quarantined lexical generation should remain inspection-only"
    );
    assert_eq!(
        summary["retained_publish_backup_retention_limit"].as_u64(),
        Some(1)
    );
}
