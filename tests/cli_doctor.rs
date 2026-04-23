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
}
