use assert_cmd::Command;
use coding_agent_search::search::tantivy::expected_index_dir;
use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use std::time::Duration;

fn cass_cmd(test_home: &Path) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("XDG_DATA_HOME", test_home)
        .env("XDG_CONFIG_HOME", test_home)
        .env("HOME", test_home);
    cmd
}

fn seed_healthy_empty_index(test_home: &Path, data_dir: &Path) {
    let out = cass_cmd(test_home)
        .args([
            "index",
            "--force-rebuild",
            "--json",
            "--data-dir",
            data_dir.to_str().expect("utf8"),
        ])
        .output()
        .expect("run seed index");
    assert!(
        out.status.success(),
        "seed index failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

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

fn write_superseded_reclaimable_manifest(generation_dir: &Path, generation_id: &str) {
    fs::create_dir_all(generation_dir).expect("create superseded generation dir");
    fs::write(
        generation_dir.join("lexical-generation-manifest.json"),
        serde_json::to_vec_pretty(&json!({
            "manifest_version": 1,
            "generation_id": generation_id,
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
                "shard_id": "shard-old",
                "shard_ordinal": 0,
                "state": "published",
                "updated_at_ms": 1_733_000_000_222_i64,
                "indexed_doc_count": 9,
                "message_count": 9,
                "artifact_bytes": 128,
                "stable_hash": "stable-hash-old",
                "reclaimable": true,
                "pinned": false,
                "recovery_reason": null,
                "quarantine_reason": null
            }],
            "merge_debt": {
                "state": "none",
                "updated_at_ms": null,
                "pending_shard_count": 0,
                "pending_artifact_bytes": 0,
                "reason": null,
                "controller_reason": null
            },
            "build_state": "validated",
            "publish_state": "superseded",
            "failure_history": []
        }))
        .expect("serialize superseded manifest"),
    )
    .expect("write superseded manifest");
}

fn write_active_manifest(generation_dir: &Path, generation_id: &str) {
    fs::create_dir_all(generation_dir).expect("create active generation dir");
    fs::write(
        generation_dir.join("lexical-generation-manifest.json"),
        serde_json::to_vec_pretty(&json!({
            "manifest_version": 1,
            "generation_id": generation_id,
            "attempt_id": "attempt-1",
            "created_at_ms": 1_733_000_000_000_i64,
            "updated_at_ms": 1_733_000_000_321_i64,
            "source_db_fingerprint": "fp-test",
            "conversation_count": 3,
            "message_count": 9,
            "indexed_doc_count": 0,
            "equivalence_manifest_fingerprint": null,
            "shard_plan": null,
            "build_budget": null,
            "shards": [{
                "shard_id": "shard-active",
                "shard_ordinal": 0,
                "state": "building",
                "updated_at_ms": 1_733_000_000_222_i64,
                "indexed_doc_count": 0,
                "message_count": 0,
                "artifact_bytes": 128,
                "stable_hash": null,
                "reclaimable": true,
                "pinned": false,
                "recovery_reason": null,
                "quarantine_reason": null
            }],
            "merge_debt": {
                "state": "none",
                "updated_at_ms": null,
                "pending_shard_count": 0,
                "pending_artifact_bytes": 0,
                "reason": null,
                "controller_reason": null
            },
            "build_state": "building",
            "publish_state": "staged",
            "failure_history": []
        }))
        .expect("serialize active manifest"),
    )
    .expect("write active manifest");
}

#[test]
fn doctor_json_surfaces_quarantine_gc_eligibility() {
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

    let out = cass_cmd(test_home.path())
        .args([
            "doctor",
            "--json",
            "--data-dir",
            data_dir.to_str().expect("utf8"),
        ])
        .env("CASS_LEXICAL_PUBLISH_BACKUP_RETENTION", "1")
        .output()
        .expect("run cass doctor --json");
    assert!(
        out.status.success(),
        "cass doctor --json failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let payload: Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    let quarantine = &payload["quarantine"];

    assert_eq!(
        quarantine["summary"]["gc_eligible_asset_count"].as_u64(),
        Some(1)
    );
    assert_eq!(
        quarantine["summary"]["inspection_required_asset_count"].as_u64(),
        Some(3)
    );
    assert_eq!(
        quarantine["summary"]["retained_publish_backup_retention_limit"].as_u64(),
        Some(1)
    );
    assert_eq!(
        quarantine["summary"]["cleanup_dry_run_generation_count"].as_u64(),
        Some(1)
    );
    assert_eq!(
        quarantine["summary"]["cleanup_dry_run_inspection_required_count"].as_u64(),
        Some(1)
    );
    assert_eq!(
        quarantine["summary"]["cleanup_apply_allowed"].as_bool(),
        Some(false)
    );

    let retained = quarantine["retained_publish_backups"]
        .as_array()
        .expect("retained publish backups array");
    assert!(
        retained.iter().any(|entry| {
            entry["path"]
                .as_str()
                .unwrap_or_default()
                .contains("prior-live-older")
                && entry["safe_to_gc"].as_bool() == Some(true)
        }),
        "older retained publish backup should be GC-eligible in doctor JSON"
    );
    assert!(
        retained.iter().any(|entry| {
            entry["path"]
                .as_str()
                .unwrap_or_default()
                .contains("prior-live-newer")
                && entry["safe_to_gc"].as_bool() == Some(false)
        }),
        "newest retained publish backup should remain protected in doctor JSON"
    );

    let generations = quarantine["lexical_generations"]
        .as_array()
        .expect("lexical generations array");
    assert_eq!(generations.len(), 1, "expected one quarantined generation");
    assert_eq!(generations[0]["generation_id"], "gen-quarantined");
    assert_eq!(generations[0]["safe_to_gc"].as_bool(), Some(false));
    assert_eq!(generations[0]["reclaimable_bytes"].as_u64(), Some(0));
    assert!(
        generations[0]["gc_reason"]
            .as_str()
            .unwrap_or_default()
            .contains("cleanup dry-run"),
        "doctor JSON should expose why quarantined lexical generations are held"
    );

    let dry_run = &quarantine["lexical_cleanup_dry_run"];
    assert_eq!(dry_run["dry_run"].as_bool(), Some(true));
    assert_eq!(
        dry_run["inventories"][0]["disposition"].as_str(),
        Some("quarantined_retained")
    );
    let apply_gate = &quarantine["lexical_cleanup_apply_gate"];
    assert_eq!(apply_gate["apply_allowed"].as_bool(), Some(false));
    assert_eq!(
        apply_gate["inspection_required_generation_ids"][0].as_str(),
        Some("gen-quarantined")
    );
}

#[test]
fn doctor_fix_prunes_safe_derivative_cleanup_candidates() {
    let test_home = tempfile::tempdir().expect("tempdir");
    let data_dir = test_home.path().join("cass-data");
    seed_healthy_empty_index(test_home.path(), &data_dir);
    let index_path = expected_index_dir(&data_dir);
    fs::create_dir_all(&index_path).expect("create expected index dir");
    let retained_publish_dir = index_path
        .parent()
        .expect("index parent")
        .join(".lexical-publish-backups");
    fs::create_dir_all(&retained_publish_dir).expect("create retained publish dir");
    let older_backup = retained_publish_dir.join("prior-live-older");
    fs::create_dir_all(&older_backup).expect("create older retained backup");
    fs::write(older_backup.join("segment-a"), b"old backup bytes")
        .expect("write older retained publish backup");
    std::thread::sleep(Duration::from_millis(20));
    let newer_backup = retained_publish_dir.join("prior-live-newer");
    fs::create_dir_all(&newer_backup).expect("create newer retained backup");
    fs::write(newer_backup.join("segment-b"), b"new backup bytes")
        .expect("write newer retained publish backup");

    let superseded_dir = index_path
        .parent()
        .expect("index parent")
        .join("generation-superseded");
    write_superseded_reclaimable_manifest(&superseded_dir, "gen-superseded");
    fs::write(
        superseded_dir.join("segment-old"),
        b"superseded generation bytes",
    )
    .expect("write superseded generation artifact");

    let quarantined_dir = index_path
        .parent()
        .expect("index parent")
        .join("generation-quarantined");
    write_quarantined_manifest(&quarantined_dir);
    fs::write(
        quarantined_dir.join("segment-a"),
        b"quarantined generation bytes",
    )
    .expect("write quarantined generation artifact");

    let out = cass_cmd(test_home.path())
        .args([
            "doctor",
            "--json",
            "--fix",
            "--data-dir",
            data_dir.to_str().expect("utf8"),
        ])
        .env("CASS_LEXICAL_PUBLISH_BACKUP_RETENTION", "1")
        .output()
        .expect("run cass doctor --json --fix");
    assert!(
        out.status.success(),
        "cass doctor --json --fix failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        !older_backup.exists(),
        "older retained publish backup outside cap should be pruned"
    );
    assert!(
        newer_backup.exists(),
        "newest retained publish backup should remain protected"
    );
    assert!(
        !superseded_dir.exists(),
        "fully reclaimable superseded lexical generation should be pruned"
    );
    assert!(
        quarantined_dir.exists(),
        "quarantined lexical generation must remain for inspection"
    );

    let payload: Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    let cleanup = &payload["cleanup_apply"];
    assert_eq!(cleanup["requested"].as_bool(), Some(true));
    assert_eq!(cleanup["applied"].as_bool(), Some(true));
    assert_eq!(cleanup["before_reclaim_candidate_count"].as_u64(), Some(1));
    assert_eq!(cleanup["after_reclaim_candidate_count"].as_u64(), Some(0));
    assert_eq!(cleanup["pruned_asset_count"].as_u64(), Some(2));
    assert!(
        cleanup["reclaimed_bytes"].as_u64().unwrap_or(0) > 0,
        "apply result should summarize reclaimed bytes"
    );
    let actions = cleanup["actions"].as_array().expect("cleanup actions");
    assert!(
        actions.iter().any(|action| {
            action["artifact_kind"].as_str() == Some("retained_publish_backup")
                && action["applied"].as_bool() == Some(true)
        }),
        "apply result should include retained publish backup prune action"
    );
    assert!(
        actions.iter().any(|action| {
            action["artifact_kind"].as_str() == Some("lexical_generation")
                && action["generation_id"].as_str() == Some("gen-superseded")
                && action["applied"].as_bool() == Some(true)
        }),
        "apply result should include superseded generation prune action"
    );
}

#[test]
fn doctor_fix_preserves_reclaimable_generations_when_active_work_exists() {
    let test_home = tempfile::tempdir().expect("tempdir");
    let data_dir = test_home.path().join("cass-data");
    seed_healthy_empty_index(test_home.path(), &data_dir);
    let index_path = expected_index_dir(&data_dir);
    fs::create_dir_all(&index_path).expect("create expected index dir");

    let superseded_dir = index_path
        .parent()
        .expect("index parent")
        .join("generation-superseded");
    write_superseded_reclaimable_manifest(&superseded_dir, "gen-superseded");
    fs::write(
        superseded_dir.join("segment-old"),
        b"superseded generation bytes",
    )
    .expect("write superseded generation artifact");

    let active_dir = index_path
        .parent()
        .expect("index parent")
        .join("generation-active");
    write_active_manifest(&active_dir, "gen-active");
    fs::write(
        active_dir.join("segment-active"),
        b"active generation bytes",
    )
    .expect("write active generation artifact");

    let out = cass_cmd(test_home.path())
        .args([
            "doctor",
            "--json",
            "--fix",
            "--data-dir",
            data_dir.to_str().expect("utf8"),
        ])
        .output()
        .expect("run cass doctor --json --fix");
    assert!(
        out.status.success(),
        "cass doctor --json --fix failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        superseded_dir.exists(),
        "cleanup apply must preserve reclaimable generations while active work exists"
    );
    assert!(
        active_dir.exists(),
        "cleanup apply must preserve active scratch/resumable work"
    );

    let payload: Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    let cleanup = &payload["cleanup_apply"];
    assert_eq!(cleanup["applied"].as_bool(), Some(false));
    assert_eq!(cleanup["pruned_asset_count"].as_u64(), Some(0));
    assert!(
        cleanup["blocked_reasons"]
            .as_array()
            .expect("blocked reasons")
            .iter()
            .any(|reason| {
                reason
                    .as_str()
                    .unwrap_or_default()
                    .contains("active generation work")
            }),
        "apply result should explain active-work safety block"
    );
}
