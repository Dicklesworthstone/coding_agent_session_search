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

fn write_generation_manifest(
    generation_dir: &Path,
    generation_id: &str,
    build_state: &str,
    publish_state: &str,
    shard_state: &str,
    pinned: bool,
    reclaimable: bool,
) {
    fs::create_dir_all(generation_dir).expect("create generation dir");
    fs::write(
        generation_dir.join("lexical-generation-manifest.json"),
        serde_json::to_vec_pretty(&json!({
            "manifest_version": 3,
            "generation_id": generation_id,
            "attempt_id": format!("{generation_id}-attempt"),
            "created_at_ms": 1_733_000_010_000_i64,
            "updated_at_ms": 1_733_000_010_321_i64,
            "source_db_fingerprint": "fp-lifecycle-test",
            "conversation_count": 4,
            "message_count": 12,
            "indexed_doc_count": 12,
            "equivalence_manifest_fingerprint": "eq-lifecycle-test",
            "shard_plan": null,
            "build_budget": null,
            "shards": [{
                "shard_id": format!("{generation_id}-shard"),
                "shard_ordinal": 0,
                "state": shard_state,
                "updated_at_ms": 1_733_000_010_222_i64,
                "indexed_doc_count": 12,
                "message_count": 12,
                "artifact_bytes": 256,
                "stable_hash": format!("{generation_id}-stable"),
                "reclaimable": reclaimable,
                "pinned": pinned,
                "recovery_reason": null,
                "quarantine_reason": if shard_state == "quarantined" { Some("validation_failed") } else { None::<&str> }
            }],
            "merge_debt": {
                "state": "none",
                "updated_at_ms": null,
                "pending_shard_count": 0,
                "pending_artifact_bytes": 0,
                "reason": null,
                "controller_reason": null
            },
            "build_state": build_state,
            "publish_state": publish_state,
            "failure_history": []
        }))
        .expect("serialize manifest"),
    )
    .expect("write manifest");
    fs::write(
        generation_dir.join("segment"),
        format!("{generation_id}-artifact-bytes"),
    )
    .expect("write generation artifact");
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
    let rebuild_progress = &payload["rebuild_progress"];

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
    assert_eq!(rebuild_progress["active"].as_bool(), Some(true));
    assert_eq!(
        rebuild_progress["processed_conversations"].as_u64(),
        Some(4)
    );
    assert_eq!(rebuild_progress["total_conversations"].as_u64(), Some(10));
    assert_eq!(
        rebuild_progress["remaining_conversations"].as_u64(),
        Some(6)
    );
    assert_eq!(rebuild_progress["completion_ratio"].as_f64(), Some(0.4));
    assert_eq!(rebuild_progress["queue_depth"].as_u64(), Some(3));
    assert_eq!(rebuild_progress["queue_capacity"].as_u64(), Some(5));
    assert_eq!(rebuild_progress["queue_headroom"].as_u64(), Some(2));
    assert_eq!(
        rebuild_progress["inflight_message_bytes"].as_u64(),
        Some(65_536)
    );
    assert_eq!(
        rebuild_progress["inflight_message_bytes_headroom"].as_u64(),
        Some(65_536)
    );
    assert_eq!(
        rebuild_progress["controller_mode"].as_str(),
        Some("pressure_limited")
    );
    assert_eq!(
        rebuild_progress["controller_reason"].as_str(),
        Some("queue_depth_3_reached_pipeline_capacity_3")
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
    let quarantine = &payload["quarantine"];
    let summary = &quarantine["summary"];

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

    let failed_seed_entries = quarantine["failed_seed_bundle_files"]
        .as_array()
        .expect("status.quarantine must expose failed seed bundle entries");
    assert_eq!(
        failed_seed_entries.len(),
        2,
        "status.quarantine must preserve full failed seed bundle inventory, not only summary"
    );
    assert!(failed_seed_entries.iter().all(|entry| {
        entry["path"]
            .as_str()
            .is_some_and(|path| path.contains(".failed-baseline-seed.bak"))
            || entry["path"].as_str().is_some_and(|path| {
                path.contains(".failed-baseline-seed.bak-wal")
                    || path.ends_with(".failed-baseline-seed.bak-wal")
            })
    }));

    let retained_backups = quarantine["retained_publish_backups"]
        .as_array()
        .expect("status.quarantine must expose retained publish backups");
    assert_eq!(
        retained_backups.len(),
        2,
        "status.quarantine must preserve retained publish backup details"
    );
    assert!(
        retained_backups
            .iter()
            .any(|entry| entry["safe_to_gc"].as_bool() == Some(true)),
        "one retained publish backup should be GC-eligible outside the cap"
    );
    assert!(
        retained_backups
            .iter()
            .any(|entry| entry["safe_to_gc"].as_bool() == Some(false)),
        "one retained publish backup should remain protected by the cap"
    );

    let lexical_generations = quarantine["lexical_generations"]
        .as_array()
        .expect("status.quarantine must expose lexical generation inventory");
    assert_eq!(lexical_generations.len(), 1);
    assert_eq!(
        lexical_generations[0]["generation_id"].as_str(),
        Some("gen-quarantined")
    );
    assert_eq!(
        lexical_generations[0]["publish_state"].as_str(),
        Some("quarantined")
    );

    let inspection_artifacts = quarantine["quarantined_artifacts"]
        .as_array()
        .expect("status.quarantine must expose flattened quarantined artifacts");
    assert!(
        inspection_artifacts.iter().any(|entry| {
            entry["artifact_kind"].as_str() == Some("lexical_generation")
                && entry["generation_id"].as_str() == Some("gen-quarantined")
        }),
        "status.quarantine must include the quarantined lexical generation artifact"
    );
    assert!(
        inspection_artifacts.iter().any(|entry| {
            entry["artifact_kind"].as_str() == Some("lexical_shard")
                && entry["generation_id"].as_str() == Some("gen-quarantined")
                && entry["shard_id"].as_str() == Some("shard-a")
        }),
        "status.quarantine must include the quarantined shard artifact"
    );
}

#[test]
fn status_json_surfaces_lexical_generation_lifecycle_inventory() {
    let test_home = tempfile::tempdir().expect("tempdir");
    let data_dir = test_home.path().join("cass-data");
    let index_path = expected_index_dir(&data_dir);
    fs::create_dir_all(&index_path).expect("create expected index dir");
    let generation_root = index_path.parent().expect("index parent");

    write_generation_manifest(
        &generation_root.join("generation-current"),
        "gen-current",
        "validated",
        "published",
        "published",
        true,
        false,
    );
    write_generation_manifest(
        &generation_root.join("generation-staged"),
        "gen-staged",
        "built",
        "staged",
        "staged",
        false,
        true,
    );
    write_generation_manifest(
        &generation_root.join("generation-failed"),
        "gen-failed",
        "failed",
        "staged",
        "abandoned",
        false,
        true,
    );
    write_generation_manifest(
        &generation_root.join("generation-superseded"),
        "gen-superseded",
        "validated",
        "superseded",
        "published",
        false,
        true,
    );
    write_generation_manifest(
        &generation_root.join("generation-quarantined"),
        "gen-quarantined",
        "failed",
        "quarantined",
        "quarantined",
        false,
        false,
    );

    let out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args([
            "status",
            "--json",
            "--data-dir",
            data_dir.to_str().expect("utf8"),
        ])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
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

    assert_eq!(summary["lexical_generation_count"].as_u64(), Some(5));
    assert_eq!(
        summary["lexical_generation_publish_state_counts"]["published"].as_u64(),
        Some(1)
    );
    assert_eq!(
        summary["lexical_generation_publish_state_counts"]["staged"].as_u64(),
        Some(2)
    );
    assert_eq!(
        summary["lexical_generation_publish_state_counts"]["superseded"].as_u64(),
        Some(1)
    );
    assert_eq!(
        summary["lexical_generation_publish_state_counts"]["quarantined"].as_u64(),
        Some(1)
    );
    assert_eq!(
        summary["lexical_generation_build_state_counts"]["validated"].as_u64(),
        Some(2)
    );
    assert_eq!(
        summary["lexical_generation_build_state_counts"]["built"].as_u64(),
        Some(1)
    );
    assert_eq!(
        summary["lexical_generation_build_state_counts"]["failed"].as_u64(),
        Some(2)
    );
    assert_eq!(
        summary["lexical_quarantined_generation_count"].as_u64(),
        Some(1)
    );
}
