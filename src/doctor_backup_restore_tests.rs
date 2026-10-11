//! Real-engine coverage for manifest-bound doctor backup restoration.

use super::*;
use crate::franken_sync::Connection;
use serde_json::{Value, json};
use tempfile::TempDir;

const WAL_MARKER: &str = "private-committed-wal-message-do-not-print";
const BACKUP_ID: &str = "wal_restore_fixture";

#[derive(Clone, Copy, Debug)]
enum ShmFixture {
    Missing,
    Stale,
}

struct BackupFixture {
    root: TempDir,
    data_dir: PathBuf,
    backup_dir: PathBuf,
    backup_db: PathBuf,
    backup_wal: PathBuf,
    manifest_path: PathBuf,
    table_root: u64,
}

fn create_probe_database(path: &Path, with_wal: bool) -> u64 {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let connection = Connection::open(path.to_string_lossy().into_owned()).unwrap();
    connection.query("PRAGMA journal_mode = WAL;").unwrap();
    connection
        .execute("PRAGMA wal_autocheckpoint = 0;")
        .unwrap();
    connection
        .execute("CREATE TABLE restore_probe(id INTEGER PRIMARY KEY, marker TEXT NOT NULL);")
        .unwrap();
    connection
        .execute("INSERT INTO restore_probe VALUES (1, 'checkpointed');")
        .unwrap();
    let table_root = u64::try_from(
        connection
            .query_row("SELECT rootpage FROM sqlite_master WHERE name = 'restore_probe';")
            .unwrap()
            .get_typed::<i64>(0)
            .unwrap(),
    )
    .unwrap();
    connection
        .query("PRAGMA wal_checkpoint(TRUNCATE);")
        .unwrap();
    if with_wal {
        connection
            .execute_with_params(
                "INSERT INTO restore_probe VALUES (2, ?1);",
                &[crate::franken_sync::SqliteValue::Text(WAL_MARKER.into())],
            )
            .unwrap();
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM restore_probe;")
                .unwrap()
                .get_typed::<i64>(0)
                .unwrap(),
            2
        );
    }
    connection.close_without_checkpoint().unwrap();
    table_root
}

impl BackupFixture {
    fn new(with_wal: bool, shm: ShmFixture) -> Self {
        let root = tempfile::tempdir().unwrap();
        let data_dir = root.path().join("cass-data");
        let source = root.path().join("seed.db");
        let table_root = create_probe_database(&source, with_wal);
        let backup_dir = data_dir
            .join("doctor/candidate-promotions")
            .join(BACKUP_ID)
            .join("backup");
        let backup_db = backup_dir.join("live/agent_search.db");
        let backup_wal = doctor_sqlite_sidecar_path(&backup_db, "-wal").unwrap();
        std::fs::create_dir_all(backup_db.parent().unwrap()).unwrap();
        std::fs::copy(&source, &backup_db).unwrap();
        if with_wal {
            let source_wal = doctor_sqlite_sidecar_path(&source, "-wal").unwrap();
            assert!(
                std::fs::metadata(&source_wal).unwrap().len() > 32,
                "fixture must retain committed WAL frames"
            );
            std::fs::copy(source_wal, &backup_wal).unwrap();
            if matches!(shm, ShmFixture::Stale) {
                // An invalid derived WAL-index header requires the pinned
                // engine's explicit SHM recovery, without changing DB/WAL.
                std::fs::write(
                    doctor_sqlite_sidecar_path(&backup_db, "-shm").unwrap(),
                    [0u8; 32 * 1024],
                )
                .unwrap();
            }
        }
        let mut artifacts = Vec::new();
        for (suffix, role) in [
            ("", "prior_live_archive_db_backup"),
            ("-wal", "prior_live_archive_wal_backup"),
            ("-shm", "prior_live_archive_shm_backup"),
        ] {
            let path = if suffix.is_empty() {
                backup_db.clone()
            } else {
                doctor_sqlite_sidecar_path(&backup_db, suffix).unwrap()
            };
            if path.exists() {
                artifacts.push(json!({
                    "artifact_kind": role,
                    "backup_path": path,
                    "checksum_blake3": file_blake3_hex(&path).unwrap(),
                    "size_bytes": std::fs::metadata(&path).unwrap().len(),
                }));
            }
        }
        let manifest_path = backup_dir.join("manifest.json");
        let manifest = json!({
            "schema_version": 1,
            "manifest_kind": DOCTOR_BACKUP_MANIFEST_KIND,
            "promotion_id": BACKUP_ID,
            "backup_dir": backup_dir,
            "artifacts": artifacts,
        });
        std::fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        Self {
            root,
            data_dir,
            backup_dir,
            backup_db,
            backup_wal,
            manifest_path,
            table_root,
        }
    }

    fn manifest(&self) -> Value {
        serde_json::from_slice(&std::fs::read(&self.manifest_path).unwrap()).unwrap()
    }

    fn write_manifest(&self, manifest: &Value) {
        std::fs::write(
            &self.manifest_path,
            serde_json::to_vec_pretty(manifest).unwrap(),
        )
        .unwrap();
    }

    fn verify(&self) -> DoctorBackupVerification {
        let record =
            doctor_backup_manifest_record_from_path(&self.data_dir, &self.manifest_path).unwrap();
        verify_doctor_backup_record(&self.data_dir, record)
    }

    fn seed_live_database(&self) -> PathBuf {
        let path = self.data_dir.join("agent_search.db");
        let seed = self.root.path().join("live-seed.db");
        create_probe_database(&seed, false);
        std::fs::copy(seed, &path).unwrap();
        path
    }
}

fn tree_hashes(root: &Path) -> BTreeMap<PathBuf, String> {
    fn visit(root: &Path, path: &Path, files: &mut BTreeMap<PathBuf, String>) {
        for entry in std::fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                visit(root, &path, files);
            } else {
                files.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    file_blake3_hex(&path).unwrap(),
                );
            }
        }
    }
    let mut files = BTreeMap::new();
    visit(root, root, &mut files);
    files
}

fn bundle_bytes(path: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    ["", "-wal", "-shm", "-journal"]
        .into_iter()
        .map(|suffix| {
            let path = if suffix.is_empty() {
                path.to_path_buf()
            } else {
                doctor_sqlite_sidecar_path(path, suffix).unwrap()
            };
            let bytes = std::fs::read(&path).ok();
            (path, bytes)
        })
        .collect()
}

fn probe_markers(path: &Path) -> Vec<(i64, String)> {
    // This helper reads only the private control, rehearsed candidate, or
    // already-promoted live target. It never opens the stored backup source.
    let connection =
        Connection::open_schema_only_with_wal_index_recovery(path.to_string_lossy().into_owned())
            .unwrap();
    let rows = connection
        .query("SELECT id, marker FROM restore_probe ORDER BY id;")
        .unwrap()
        .into_iter()
        .map(|row| (row.get_typed(0).unwrap(), row.get_typed(1).unwrap()))
        .collect();
    connection.close_without_checkpoint().unwrap();
    rows
}

#[test]
#[serial_test::serial]
fn doctor_backup_restore_keeps_wal_rows_and_recovers_only_private_shm() {
    for shm in [ShmFixture::Missing, ShmFixture::Stale] {
        let fixture = BackupFixture::new(true, shm);
        let live = fixture.seed_live_database();
        let live_before = bundle_bytes(&live);
        let backup_before = tree_hashes(&fixture.backup_dir);
        let db_only = fixture.root.path().join("db-only-control.db");
        std::fs::copy(&fixture.backup_db, &db_only).unwrap();
        assert_eq!(
            probe_markers(&db_only),
            vec![(1, "checkpointed".to_string())]
        );

        let verification = fixture.verify();
        assert_eq!(verification.status, "verified", "{verification:#?}");
        assert!(verification.restore_rehearsal_allowed);
        assert!(matches!(
            verification.storage_probe_status,
            "passed" | "requires_private_wal_index_recovery"
        ));
        assert_eq!(tree_hashes(&fixture.backup_dir), backup_before);
        let (plan, fingerprint) = doctor_restore_plan_payload(
            &fixture.data_dir,
            &live,
            BACKUP_ID,
            &verification,
            false,
            None,
        );
        let rehearsal = run_doctor_backup_restore_rehearsal(
            &fixture.data_dir,
            &live,
            BACKUP_ID,
            &verification,
            &plan,
        );
        assert_eq!(rehearsal["status"], "passed", "{rehearsal:#}");
        assert_eq!(rehearsal["storage_probe_status"], "passed");
        let candidate = PathBuf::from(rehearsal["candidate_db_path"].as_str().unwrap());
        assert!(
            doctor_sqlite_sidecar_path(&candidate, "-wal")
                .unwrap()
                .exists()
        );
        assert!(!candidate.with_file_name("candidate.db-wal").exists());
        let receipt: Value = serde_json::from_slice(
            &std::fs::read(rehearsal["receipt_path"].as_str().unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(
            receipt["checksum_set"]["candidate/agent_search.db-wal"],
            verification.prior_live_wal_blake3.as_deref().unwrap()
        );
        let candidate_shm = doctor_sqlite_sidecar_path(&candidate, "-shm").unwrap();
        if candidate_shm.exists() {
            assert_eq!(
                receipt["checksum_set"]["candidate/agent_search.db-shm"],
                file_blake3_hex(&candidate_shm).unwrap()
            );
        }
        assert_eq!(
            probe_markers(&candidate),
            vec![(1, "checkpointed".to_string()), (2, WAL_MARKER.to_string())]
        );
        assert_eq!(bundle_bytes(&live), live_before);
        assert_eq!(tree_hashes(&fixture.backup_dir), backup_before);
        assert!(!rehearsal.to_string().contains(WAL_MARKER));

        let applied = run_doctor_backup_restore_apply(
            &fixture.data_dir,
            &live,
            BACKUP_ID,
            &verification,
            &fingerprint,
            &fingerprint,
        );
        assert_eq!(applied["status"], "applied", "{applied:#}");
        assert_eq!(
            probe_markers(&live),
            vec![(1, "checkpointed".to_string()), (2, WAL_MARKER.to_string())]
        );
        assert_eq!(tree_hashes(&fixture.backup_dir), backup_before);
        assert!(!applied.to_string().contains(WAL_MARKER));
        eprintln!(
            "{}",
            json!({"event":"doctor_backup_wal_round_trip", "source_shm": format!("{shm:?}"), "restored_rows":2})
        );
    }
}

#[test]
#[serial_test::serial]
fn doctor_backup_verification_refuses_incomplete_or_changed_wal_authority() {
    for case in [
        "changed",
        "missing",
        "missing_hash",
        "missing_path",
        "unrecorded",
        "misbound",
        "unrecorded_shm",
        "rollback_journal",
    ] {
        let fixture = BackupFixture::new(true, ShmFixture::Missing);
        let live = fixture.seed_live_database();
        let live_before = bundle_bytes(&live);
        assert_eq!(fixture.verify().status, "verified", "valid fixture: {case}");
        let mut manifest = fixture.manifest();
        let artifacts = manifest["artifacts"].as_array_mut().unwrap();
        let wal_index = artifacts
            .iter()
            .position(|value| value["artifact_kind"] == "prior_live_archive_wal_backup")
            .unwrap();
        match case {
            "changed" => OpenOptions::new()
                .append(true)
                .open(&fixture.backup_wal)
                .unwrap()
                .write_all(b"untrusted-wal-tail")
                .unwrap(),
            "missing" => std::fs::rename(
                &fixture.backup_wal,
                fixture.root.path().join("retained.wal"),
            )
            .unwrap(),
            "missing_hash" => {
                artifacts[wal_index]
                    .as_object_mut()
                    .unwrap()
                    .remove("checksum_blake3");
            }
            "missing_path" => {
                artifacts[wal_index]
                    .as_object_mut()
                    .unwrap()
                    .remove("backup_path");
            }
            "unrecorded" => {
                artifacts.remove(wal_index);
            }
            "misbound" => {
                let renamed = fixture.backup_db.with_file_name("unrelated.db-wal");
                std::fs::rename(&fixture.backup_wal, &renamed).unwrap();
                artifacts[wal_index]["backup_path"] = json!(renamed);
            }
            "unrecorded_shm" => std::fs::write(
                doctor_sqlite_sidecar_path(&fixture.backup_db, "-shm").unwrap(),
                [0u8; 32 * 1024],
            )
            .unwrap(),
            "rollback_journal" => std::fs::write(
                doctor_sqlite_sidecar_path(&fixture.backup_db, "-journal").unwrap(),
                b"unrecorded rollback state",
            )
            .unwrap(),
            _ => unreachable!(),
        }
        fixture.write_manifest(&manifest);
        let backup_before = tree_hashes(&fixture.backup_dir);
        let verification = fixture.verify();
        assert_eq!(verification.status, "failed", "{case}: {verification:#?}");
        assert!(!verification.restore_rehearsal_allowed, "{case}");
        assert!(!verification.blocked_reasons.is_empty(), "{case}");
        assert_eq!(tree_hashes(&fixture.backup_dir), backup_before);
        let target_root = fixture.root.path().join("refused-copy");
        assert!(
            copy_doctor_backup_restore_bundle(
                &verification,
                &target_root,
                &target_root.join("candidate.db")
            )
            .is_err(),
            "{case}"
        );
        assert!(
            !target_root.exists(),
            "unverified copy must stop before creating output: {case}"
        );
        let applied = run_doctor_backup_restore_apply(
            &fixture.data_dir,
            &live,
            BACKUP_ID,
            &verification,
            "matching-plan",
            "matching-plan",
        );
        assert_eq!(applied["status"], "refused", "{case}: {applied:#}");
        assert_eq!(applied["reason"], "backup_verification_failed");
        assert_eq!(bundle_bytes(&live), live_before);
        assert_eq!(tree_hashes(&fixture.backup_dir), backup_before);
    }
}

#[test]
#[serial_test::serial]
fn doctor_backup_verification_refuses_duplicate_restore_roles() {
    for role in [
        "prior_live_archive_db_backup",
        "prior_live_archive_wal_backup",
        "prior_live_archive_shm_backup",
    ] {
        let fixture = BackupFixture::new(true, ShmFixture::Stale);
        let mut manifest = fixture.manifest();
        let artifacts = manifest["artifacts"].as_array_mut().unwrap();
        let duplicate = artifacts
            .iter()
            .find(|artifact| artifact["artifact_kind"] == role)
            .unwrap()
            .clone();
        artifacts.push(duplicate);
        fixture.write_manifest(&manifest);
        let backup_before = tree_hashes(&fixture.backup_dir);

        let verification = fixture.verify();
        assert_eq!(verification.status, "failed", "{role}: {verification:#?}");
        assert!(verification.blocked_reasons.iter().any(|reason| {
            reason.contains("repeats restore component") && reason.contains(role)
        }));
        assert!(!verification.restore_rehearsal_allowed);
        let target_root = fixture.root.path().join("refused-duplicate");
        assert!(
            copy_doctor_backup_restore_bundle(
                &verification,
                &target_root,
                &target_root.join("candidate.db"),
            )
            .is_err()
        );
        assert!(!target_root.exists());
        assert_eq!(tree_hashes(&fixture.backup_dir), backup_before);
    }
}

#[test]
#[serial_test::serial]
fn doctor_backup_restore_rechecks_source_authority_at_copy_time() {
    for case in ["changed_wal", "missing_wal", "new_wal"] {
        let fixture = BackupFixture::new(case != "new_wal", ShmFixture::Missing);
        let live = fixture.seed_live_database();
        let live_before = bundle_bytes(&live);
        let verification = fixture.verify();
        assert_eq!(verification.status, "verified", "{case}: {verification:#?}");
        let verified_wal_hash = verification.prior_live_wal_blake3.clone();
        match case {
            "changed_wal" => OpenOptions::new()
                .append(true)
                .open(&fixture.backup_wal)
                .unwrap()
                .write_all(b"changed-after-verification")
                .unwrap(),
            "missing_wal" => std::fs::rename(
                &fixture.backup_wal,
                fixture.root.path().join("retained.wal"),
            )
            .unwrap(),
            "new_wal" => {
                let donor = BackupFixture::new(true, ShmFixture::Missing);
                std::fs::copy(&donor.backup_wal, &fixture.backup_wal).unwrap();
            }
            _ => unreachable!(),
        }
        let backup_before = tree_hashes(&fixture.backup_dir);
        let promotions_before = tree_hashes(&doctor_candidate_promotion_root(&fixture.data_dir));
        let target_root = fixture.root.path().join("drift-copy");
        assert!(
            copy_doctor_backup_restore_bundle(
                &verification,
                &target_root,
                &target_root.join("candidate.db"),
            )
            .is_err(),
            "{case}"
        );
        assert_eq!(verification.prior_live_wal_blake3, verified_wal_hash);
        let applied = run_doctor_backup_restore_apply(
            &fixture.data_dir,
            &live,
            BACKUP_ID,
            &verification,
            "matching-plan",
            "matching-plan",
        );
        assert_eq!(applied["status"], "blocked", "{case}: {applied:#}");
        assert_eq!(applied["reason"], "restore_candidate_build_failed");
        assert!(
            !Path::new(applied["candidate_dir"].as_str().unwrap())
                .join("manifest.json")
                .exists()
        );
        assert_eq!(bundle_bytes(&live), live_before);
        assert_eq!(tree_hashes(&fixture.backup_dir), backup_before);
        assert_eq!(
            tree_hashes(&doctor_candidate_promotion_root(&fixture.data_dir)),
            promotions_before
        );
    }
}

#[test]
#[serial_test::serial]
fn doctor_backup_private_read_failure_blocks_rehearsal_and_apply_before_publication() {
    let fixture = BackupFixture::new(false, ShmFixture::Missing);
    let live = fixture.seed_live_database();
    let live_before = bundle_bytes(&live);
    assert_eq!(fixture.verify().status, "verified");

    // Keep sqlite_master readable but damage the table leaf's first cell
    // pointer. A matching manifest proves file identity, not database health;
    // the private engine quick_check must reject it before promotion.
    let mut database = std::fs::read(&fixture.backup_db).unwrap();
    let page_size = u16::from_be_bytes([database[16], database[17]]);
    let page_size = if page_size == 1 {
        65_536
    } else {
        usize::from(page_size)
    };
    assert!(fixture.table_root > 1);
    let page_offset = usize::try_from(fixture.table_root - 1).unwrap() * page_size;
    assert_eq!(database[page_offset], 0x0d, "fixture uses a table leaf");
    assert!(u16::from_be_bytes([database[page_offset + 3], database[page_offset + 4]]) > 0);
    database[page_offset + 8..page_offset + 10].copy_from_slice(&1u16.to_be_bytes());
    std::fs::write(&fixture.backup_db, &database).unwrap();
    let mut manifest = fixture.manifest();
    let db_artifact = manifest["artifacts"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|artifact| artifact["artifact_kind"] == "prior_live_archive_db_backup")
        .unwrap();
    db_artifact["checksum_blake3"] = json!(file_blake3_hex(&fixture.backup_db).unwrap());
    fixture.write_manifest(&manifest);
    let backup_before = tree_hashes(&fixture.backup_dir);
    let promotions_before = tree_hashes(&doctor_candidate_promotion_root(&fixture.data_dir));

    let verification = fixture.verify();
    assert_eq!(verification.status, "verified", "{verification:#?}");
    let (plan, fingerprint) = doctor_restore_plan_payload(
        &fixture.data_dir,
        &live,
        BACKUP_ID,
        &verification,
        false,
        None,
    );
    let rehearsal = run_doctor_backup_restore_rehearsal(
        &fixture.data_dir,
        &live,
        BACKUP_ID,
        &verification,
        &plan,
    );
    assert_eq!(rehearsal["status"], "blocked", "{rehearsal:#}");
    assert_eq!(rehearsal["storage_probe_status"], "failed");
    assert!(
        rehearsal["blocked_reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|reason| reason.as_str().unwrap().contains("quick_check"))
    );

    let applied = run_doctor_backup_restore_apply(
        &fixture.data_dir,
        &live,
        BACKUP_ID,
        &verification,
        &fingerprint,
        &fingerprint,
    );
    assert_eq!(applied["status"], "blocked", "{applied:#}");
    assert_eq!(applied["reason"], "restore_candidate_build_failed");
    assert!(
        applied["blocked_reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|reason| reason.as_str().unwrap().contains("quick_check"))
    );
    assert!(
        !Path::new(applied["candidate_dir"].as_str().unwrap())
            .join("manifest.json")
            .exists()
    );
    assert_eq!(bundle_bytes(&live), live_before);
    assert_eq!(tree_hashes(&fixture.backup_dir), backup_before);
    assert_eq!(
        tree_hashes(&doctor_candidate_promotion_root(&fixture.data_dir)),
        promotions_before
    );
    assert!(!rehearsal.to_string().contains(WAL_MARKER));
    assert!(!applied.to_string().contains(WAL_MARKER));
}
