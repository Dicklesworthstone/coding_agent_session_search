use assert_cmd::Command;
use coding_agent_search::search::tantivy::expected_index_dir;
use serde_json::{Value, json};
use std::fs;
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

#[test]
fn diag_json_quarantine_surfaces_retained_artifacts() {
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
            "diag",
            "--json",
            "--quarantine",
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
        .expect("run cass diag --json --quarantine");
    assert!(
        out.status.success(),
        "cass diag --json --quarantine failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let payload: Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    let quarantine = &payload["quarantine"];

    assert_eq!(
        quarantine["summary"]["failed_seed_bundle_count"].as_u64(),
        Some(2),
        "failed seed bundle quarantine should inventory the main bundle and WAL sidecar"
    );
    assert_eq!(
        quarantine["summary"]["retained_publish_backup_count"].as_u64(),
        Some(2),
        "retained publish backup count should surface derivative lexical backups"
    );
    assert_eq!(
        quarantine["summary"]["retained_publish_backup_retention_limit"].as_u64(),
        Some(1),
        "summary should expose the active lexical publish backup retention cap"
    );
    assert_eq!(
        quarantine["summary"]["lexical_quarantined_generation_count"].as_u64(),
        Some(1),
        "quarantined lexical generation count should surface manifest-backed retained generations"
    );
    assert_eq!(
        quarantine["summary"]["lexical_quarantined_shard_count"].as_u64(),
        Some(1),
        "quarantined shard count should roll up shard-level inspection state"
    );
    assert!(
        quarantine["summary"]["total_retained_bytes"]
            .as_u64()
            .unwrap_or(0)
            > 0,
        "quarantine surface should include retained bytes"
    );
    assert_eq!(
        quarantine["summary"]["gc_eligible_asset_count"].as_u64(),
        Some(1),
        "only the older retained publish backup should be immediately GC-eligible"
    );
    assert!(
        quarantine["summary"]["gc_eligible_bytes"]
            .as_u64()
            .unwrap_or(0)
            > 0,
        "GC-eligible byte accounting should be non-zero when a retained backup falls outside cap"
    );
    assert_eq!(
        quarantine["summary"]["inspection_required_asset_count"].as_u64(),
        Some(3),
        "failed seed bundle files and quarantined lexical generation remain inspection-only"
    );
    assert_eq!(
        quarantine["summary"]["cleanup_dry_run_generation_count"].as_u64(),
        Some(1),
        "cleanup dry-run should inventory manifest-backed lexical generations"
    );
    assert_eq!(
        quarantine["summary"]["cleanup_dry_run_inspection_required_count"].as_u64(),
        Some(1),
        "cleanup dry-run should expose inspection-required lexical artifacts"
    );
    assert_eq!(
        quarantine["summary"]["cleanup_apply_allowed"].as_bool(),
        Some(false),
        "robot diagnostics must not imply cleanup apply is allowed without approval"
    );
    assert!(
        quarantine["summary"]["cleanup_dry_run_approval_fingerprint"]
            .as_str()
            .unwrap_or_default()
            .starts_with("cleanup-v1-"),
        "cleanup dry-run summary should carry the approval fingerprint"
    );

    let failed_seed_entries = quarantine["failed_seed_bundle_files"]
        .as_array()
        .expect("failed seed bundle files array");
    assert!(
        failed_seed_entries
            .iter()
            .all(|entry| entry["safe_to_gc"].as_bool() == Some(false)),
        "failed baseline seed quarantine must not be auto-GCable"
    );
    assert!(
        failed_seed_entries.iter().all(|entry| {
            entry.get("age_seconds").is_some() && entry.get("last_read_at_ms").is_some()
        }),
        "failed seed bundle entries should expose age and last-read fields"
    );
    assert!(
        failed_seed_entries.iter().any(|entry| entry["path"]
            .as_str()
            .unwrap_or_default()
            .contains(".failed-baseline-seed.bak")),
        "failed seed bundle inventory should preserve the quarantine naming pattern"
    );
    let inspection_artifacts = quarantine["quarantined_artifacts"]
        .as_array()
        .expect("flattened quarantined artifacts array");
    assert_eq!(
        inspection_artifacts.len(),
        4,
        "inspection API should enumerate failed seed bundle files, quarantined generations, and quarantined shards"
    );
    assert!(
        inspection_artifacts.iter().all(|entry| {
            entry["gc_reason"].as_str().is_some()
                && entry["path"].as_str().is_some()
                && entry.get("age_seconds").is_some()
                && entry.get("last_read_at_ms").is_some()
        }),
        "every quarantined inspection artifact should carry path, age, last-read, and gc_reason"
    );
    assert!(
        inspection_artifacts.iter().any(|entry| {
            entry["artifact_kind"].as_str() == Some("failed_seed_bundle_file")
                && entry["safe_to_gc"].as_bool() == Some(false)
                && entry["gc_reason"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("operator inspection")
        }),
        "failed seed bundle quarantine should appear in the flattened inspection API"
    );
    assert!(
        inspection_artifacts.iter().any(|entry| {
            entry["artifact_kind"].as_str() == Some("lexical_generation")
                && entry["generation_id"].as_str() == Some("gen-quarantined")
                && entry["publish_state"].as_str() == Some("quarantined")
                && entry["safe_to_gc"].as_bool() == Some(false)
                && entry["gc_reason"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("inspection")
        }),
        "quarantined lexical generations should appear in the flattened inspection API"
    );
    assert!(
        inspection_artifacts.iter().any(|entry| {
            entry["artifact_kind"].as_str() == Some("lexical_shard")
                && entry["generation_id"].as_str() == Some("gen-quarantined")
                && entry["shard_id"].as_str() == Some("shard-a")
                && entry["shard_state"].as_str() == Some("quarantined")
                && entry["gc_reason"].as_str() == Some("validation_failed")
        }),
        "quarantined shard artifacts should be individually inspectable with their gc reason"
    );

    let retained_backups = quarantine["retained_publish_backups"]
        .as_array()
        .expect("retained publish backups array");
    assert_eq!(
        retained_backups.len(),
        2,
        "expected two retained publish backups"
    );
    assert!(
        retained_backups
            .iter()
            .any(|entry| entry["safe_to_gc"].as_bool() == Some(true)),
        "one retained publish backup should be outside the retention cap"
    );
    assert!(
        retained_backups
            .iter()
            .any(|entry| entry["safe_to_gc"].as_bool() == Some(false)),
        "the newest retained publish backup should remain protected by retention"
    );

    let generations = quarantine["lexical_generations"]
        .as_array()
        .expect("lexical generations array");
    assert_eq!(generations.len(), 1, "expected one quarantined generation");
    assert_eq!(
        generations[0]["generation_id"].as_str(),
        Some("gen-quarantined")
    );
    assert_eq!(
        generations[0]["publish_state"].as_str(),
        Some("quarantined")
    );
    assert_eq!(generations[0]["quarantined_shards"].as_u64(), Some(1));
    assert_eq!(generations[0]["inspection_required"].as_bool(), Some(true));
    assert_eq!(generations[0]["safe_to_gc"].as_bool(), Some(false));
    assert_eq!(generations[0]["reclaimable_bytes"].as_u64(), Some(0));
    assert!(
        generations[0].get("age_seconds").is_some()
            && generations[0].get("last_read_at_ms").is_some(),
        "lexical generation entries should expose age and last-read fields"
    );

    let dry_run = &quarantine["lexical_cleanup_dry_run"];
    assert_eq!(dry_run["dry_run"].as_bool(), Some(true));
    assert_eq!(dry_run["generation_count"].as_u64(), Some(1));
    assert_eq!(dry_run["inspection_required_count"].as_u64(), Some(1));
    assert_eq!(
        dry_run["quarantined_generation_ids"][0].as_str(),
        Some("gen-quarantined")
    );
    assert_eq!(
        dry_run["inventories"][0]["disposition"].as_str(),
        Some("quarantined_retained"),
        "dry-run inventories should preserve lifecycle disposition"
    );
    assert!(
        dry_run["inventories"][0]["retain_until_ms"].is_null(),
        "quarantined generations should expose an indefinite retention window"
    );
    assert!(
        dry_run["inventories"][0]["retention_reason"]
            .as_str()
            .unwrap_or_default()
            .contains("operator inspection"),
        "dry-run inventories should explain the quarantine retention hold"
    );

    let apply_gate = &quarantine["lexical_cleanup_apply_gate"];
    assert_eq!(apply_gate["dry_run"].as_bool(), Some(true));
    assert_eq!(apply_gate["apply_allowed"].as_bool(), Some(false));
    assert_eq!(
        apply_gate["inspection_required_generation_ids"][0].as_str(),
        Some("gen-quarantined")
    );
    let blocker_codes = apply_gate["blocker_codes"]
        .as_array()
        .expect("blocker codes");
    assert!(
        blocker_codes
            .iter()
            .any(|code| code.as_str() == Some("operator_approval_required")),
        "apply gate should make the approval blocker machine-readable"
    );
}

// ========================================================================
// Bead coding_agent_session_search-p1x0z (child of ibuuh.10,
// /testing-metamorphic slice: cross-command quarantine consistency).
//
// `cass diag --json --quarantine` and `cass doctor --json` both expose
// a `quarantine.summary` subtree. The two subtrees are sourced from
// the same underlying state (lexical generations, retained publish
// backups, failed seed bundles, cleanup dry-run approval gate), so
// every shared field MUST agree — any divergence is a regression that
// would mislead an operator polling either surface.
//
// The sibling test above pins the diag surface alone on a seeded-
// quarantined-generation state. This test pins the CROSS-command
// invariant on a clean empty data-dir: both surfaces must report
// identical zero-valued summaries plus identical structural invariants
// (e.g. retained_publish_backup_retention_limit derived from the env
// var default).
//
// Deliberately narrow: only fields present in BOTH surfaces are
// compared. If a future change adds a quarantine field to only one
// command, that is a deliberate choice and this test will not trip
// (because the field won't have a counterpart to diff against). The
// invariant pinned is "overlap must agree", not "schemas must be
// identical".
// ========================================================================

#[test]
fn diag_and_doctor_agree_on_quarantine_summary_on_empty_data_dir() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path();

    let diag_out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args(["diag", "--json", "--quarantine", "--data-dir"])
        .arg(data_dir)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("HOME", data_dir)
        .env("XDG_DATA_HOME", data_dir.join(".local/share"))
        .env("XDG_CONFIG_HOME", data_dir.join(".config"))
        .output()
        .expect("run cass diag");
    assert!(
        diag_out.status.success(),
        "cass diag --json --quarantine must succeed on empty data-dir; stderr: {}",
        String::from_utf8_lossy(&diag_out.stderr)
    );
    let diag_stdout = String::from_utf8_lossy(&diag_out.stdout);
    let diag_json: Value = serde_json::from_str(&diag_stdout)
        .unwrap_or_else(|err| panic!("diag JSON parse failed: {err}; stdout: {diag_stdout}"));

    let doctor_out = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args(["doctor", "--json", "--data-dir"])
        .arg(data_dir)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("HOME", data_dir)
        .env("XDG_DATA_HOME", data_dir.join(".local/share"))
        .env("XDG_CONFIG_HOME", data_dir.join(".config"))
        .output()
        .expect("run cass doctor");
    // doctor may exit non-zero on unhealthy state, but must still
    // emit a parseable JSON envelope on stdout.
    let doctor_stdout = String::from_utf8_lossy(&doctor_out.stdout);
    let doctor_json: Value = serde_json::from_str(&doctor_stdout).unwrap_or_else(|err| {
        panic!(
            "doctor JSON parse failed: {err}; stdout: {doctor_stdout}\nstderr: {}",
            String::from_utf8_lossy(&doctor_out.stderr)
        )
    });

    let diag_summary = diag_json
        .get("quarantine")
        .and_then(|q| q.get("summary"))
        .and_then(Value::as_object)
        .unwrap_or_else(|| {
            panic!("diag.quarantine.summary must be an object; diag: {diag_json}")
        });
    let doctor_summary = doctor_json
        .get("quarantine")
        .and_then(|q| q.get("summary"))
        .and_then(Value::as_object)
        .unwrap_or_else(|| {
            panic!(
                "doctor.quarantine.summary must be an object; doctor: {doctor_json}"
            )
        });

    // The set of fields we pin cross-command. Intentionally specific:
    // these are the fields an operator reads to decide whether any
    // cleanup is needed. A regression on any of them silently
    // mis-reports retained disk.
    let shared_scalar_fields = [
        "failed_seed_bundle_count",
        "retained_publish_backup_count",
        "retained_publish_backup_retention_limit",
        "lexical_generation_count",
        "lexical_quarantined_generation_count",
        "lexical_quarantined_shard_count",
        "total_retained_bytes",
        "gc_eligible_asset_count",
        "gc_eligible_bytes",
        "inspection_required_asset_count",
        "inspection_required_bytes",
        "cleanup_dry_run_generation_count",
        "cleanup_dry_run_reclaim_candidate_count",
        "cleanup_dry_run_reclaimable_bytes",
        "cleanup_dry_run_retained_bytes",
        "cleanup_dry_run_protected_generation_count",
        "cleanup_dry_run_active_generation_count",
        "cleanup_dry_run_inspection_required_count",
        "cleanup_dry_run_approval_fingerprint",
        "cleanup_apply_allowed",
    ];

    for field in shared_scalar_fields {
        let diag_val = diag_summary.get(field);
        let doctor_val = doctor_summary.get(field);
        assert_eq!(
            diag_val, doctor_val,
            "quarantine.summary.{field} must agree across diag and doctor; \
             diag={diag_val:?} doctor={doctor_val:?}"
        );
    }

    // Nested bundle: the build-state and publish-state counts sub-
    // objects must also agree. These track the lexical generation
    // lifecycle; a regression that updated one command's source of
    // truth but not the other would mismatch here.
    for bundle in ["lexical_generation_build_state_counts", "lexical_generation_publish_state_counts"] {
        assert_eq!(
            diag_summary.get(bundle),
            doctor_summary.get(bundle),
            "quarantine.summary.{bundle} must agree across diag and doctor; \
             diag={:?} doctor={:?}",
            diag_summary.get(bundle),
            doctor_summary.get(bundle)
        );
    }

    // Precondition sanity: on a fresh empty data-dir every counter is
    // zero AND cleanup_apply_allowed is false. If the seeded state
    // ever changes, update both halves together — catching the drift
    // is the whole point.
    assert_eq!(
        diag_summary.get("lexical_generation_count").and_then(Value::as_u64),
        Some(0),
        "fresh data-dir must have zero lexical generations; diag: {diag_summary:?}"
    );
    assert_eq!(
        diag_summary.get("cleanup_apply_allowed").and_then(Value::as_bool),
        Some(false),
        "fresh data-dir must have cleanup_apply_allowed=false; diag: {diag_summary:?}"
    );
}
