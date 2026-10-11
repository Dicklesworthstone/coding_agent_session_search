//! Promotion must authenticate the complete canonical DB/WAL bundle before
//! replacing live files or deciding whether derived indexes can be reused.

use super::*;
use crate::franken_sync::Connection as FrankenConnection;

struct CandidateFixture {
    _temp: tempfile::TempDir,
    data_dir: PathBuf,
    db_path: PathBuf,
    index_path: PathBuf,
    candidate_dir: PathBuf,
}

impl CandidateFixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("temporary candidate fixture");
        let data_dir = temp.path().join("cass-data");
        let db_path = data_dir.join("agent_search.db");
        let index_path = data_dir.join("index");
        let candidate_dir = doctor_candidate_root(&data_dir).join("wal-candidate");
        std::fs::create_dir_all(&index_path).expect("create live derived index");
        std::fs::create_dir_all(candidate_dir.join("database"))
            .expect("create candidate database directory");
        std::fs::write(&db_path, b"prior live archive").expect("write live archive");
        std::fs::write(
            candidate_dir.join("database/candidate.db"),
            b"candidate archive",
        )
        .expect("write candidate archive");
        Self {
            _temp: temp,
            data_dir,
            db_path,
            index_path,
            candidate_dir,
        }
    }

    fn candidate_path(&self, suffix: &str) -> PathBuf {
        self.candidate_dir
            .join(format!("database/candidate.db{suffix}"))
    }

    fn checksum_set(&self, suffixes: &[&str]) -> BTreeMap<String, String> {
        suffixes
            .iter()
            .map(|suffix| {
                (
                    format!("database/candidate.db{suffix}"),
                    file_blake3_hex(&self.candidate_path(suffix))
                        .expect("hash candidate component"),
                )
            })
            .collect()
    }

    fn write_manifest(&self, checksums: BTreeMap<String, String>) {
        write_doctor_restore_candidate_manifest(
            &self.db_path,
            &self.index_path,
            &self.candidate_dir,
            "wal-fixture",
            checksums,
        )
        .expect("write completed candidate manifest");
    }

    fn promote(&self) -> DoctorCandidatePromotionReport {
        promote_doctor_reconstruct_candidate_bundle(
            &self.data_dir,
            &self.db_path,
            &self.index_path,
            &self.candidate_dir,
        )
    }

    fn assert_refused_without_mutation(&self, reason: &str) -> DoctorCandidatePromotionReport {
        let live_before = doctor_candidate_live_inventory(&self.db_path, &self.index_path);
        let candidate_before = file_blake3_hex(&self.candidate_path(""))
            .expect("hash candidate before refused promotion");
        let report = self.promote();
        assert_eq!(report.status, "blocked", "{report:#?}");
        assert!(
            report
                .blocked_reasons
                .iter()
                .any(|text| text.contains(reason)),
            "missing refusal reason {reason:?}: {report:#?}"
        );
        assert_eq!(
            serde_json::to_value(live_before).expect("serialize live inventory"),
            serde_json::to_value(doctor_candidate_live_inventory(
                &self.db_path,
                &self.index_path,
            ))
            .expect("serialize live inventory after refusal"),
            "refusal must preserve the whole live bundle"
        );
        assert_eq!(
            file_blake3_hex(&self.candidate_path("")).expect("candidate DB must remain staged"),
            candidate_before
        );
        assert!(report.backup_manifest_path.is_none(), "{report:#?}");
        report
    }
}

#[test]
fn missing_recorded_wal_is_refused_even_when_live_has_no_wal() {
    let fixture = CandidateFixture::new();
    let mut checksums = fixture.checksum_set(&[""]);
    checksums.insert(
        "database/candidate.db-wal".to_string(),
        blake3::hash(b"missing committed candidate WAL")
            .to_hex()
            .to_string(),
    );
    fixture.write_manifest(checksums);
    assert!(
        !doctor_sqlite_sidecar_path(&fixture.db_path, "-wal")
            .expect("live WAL path")
            .exists()
    );
    let report = fixture
        .assert_refused_without_mutation("recorded in the completed manifest but is missing");
    assert!(!report.sidecar_bundle_complete, "{report:#?}");
}

#[test]
fn unrecorded_canonical_db_or_wal_is_refused() {
    for suffix in ["", "-wal"] {
        let fixture = CandidateFixture::new();
        let checksums = if suffix.is_empty() {
            BTreeMap::new()
        } else {
            std::fs::write(fixture.candidate_path(suffix), b"unrecorded committed WAL")
                .expect("write unrecorded WAL");
            fixture.checksum_set(&[""])
        };
        fixture.write_manifest(checksums);
        let report =
            fixture.assert_refused_without_mutation("has no checksum in the completed manifest");
        if suffix == "-wal" {
            assert!(!report.sidecar_bundle_complete, "{report:#?}");
        }
    }
}

#[test]
fn malformed_wal_manifest_entry_cannot_masquerade_as_absence() {
    for wal_present in [false, true] {
        let fixture = CandidateFixture::new();
        if wal_present {
            std::fs::write(fixture.candidate_path("-wal"), b"committed WAL")
                .expect("write candidate WAL");
        }
        fixture.write_manifest(fixture.checksum_set(&[""]));
        let path = fixture.candidate_dir.join("manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read manifest"))
                .expect("decode manifest");
        manifest["checksum_set"]["database/candidate.db-wal"] = serde_json::Value::Null;
        std::fs::write(
            &path,
            serde_json::to_vec(&manifest).expect("encode manifest"),
        )
        .expect("write malformed WAL entry");
        let report = fixture.assert_refused_without_mutation(if wal_present {
            "has no checksum in the completed manifest"
        } else {
            "recorded in the completed manifest but is missing"
        });
        assert!(!report.sidecar_bundle_complete, "{report:#?}");
    }
}

#[test]
fn mismatched_wal_is_refused_before_live_mutation() {
    let fixture = CandidateFixture::new();
    std::fs::write(fixture.candidate_path("-wal"), b"changed committed WAL")
        .expect("write changed candidate WAL");
    let mut checksums = fixture.checksum_set(&[""]);
    checksums.insert(
        "database/candidate.db-wal".to_string(),
        blake3::hash(b"original committed WAL").to_hex().to_string(),
    );
    fixture.write_manifest(checksums);
    let report = fixture.assert_refused_without_mutation("checksum drifted after staging");
    assert!(!report.sidecar_bundle_complete, "{report:#?}");
}

#[test]
fn unreadable_live_wal_never_authorizes_index_reuse() {
    let db_hash = blake3::hash(b"unchanged DB").to_hex().to_string();
    let live = DoctorCandidateLiveInventory {
        db_exists: true,
        db_blake3: Some(db_hash.clone()),
        db_wal_exists: true,
        db_wal_blake3: None,
        index_exists: true,
        ..DoctorCandidateLiveInventory::default()
    };
    for candidate_wal in [None, Some("verified candidate WAL hash")] {
        let (_, lexical, semantic) =
            doctor_candidate_promotion_derived_status(&live, Some(&db_hash), candidate_wal);
        assert!(
            lexical && semantic,
            "an unknown live WAL is not an identity match"
        );
    }
}

#[test]
fn matching_db_and_wal_allow_index_reuse_without_candidate_shm() {
    let fixture = CandidateFixture::new();
    std::fs::copy(&fixture.db_path, fixture.candidate_path("")).expect("stage identical base DB");
    let live_wal = doctor_sqlite_sidecar_path(&fixture.db_path, "-wal").expect("live WAL path");
    let live_shm = doctor_sqlite_sidecar_path(&fixture.db_path, "-shm").expect("live SHM path");
    std::fs::write(&live_wal, b"same committed WAL").expect("write live WAL");
    std::fs::write(&live_shm, b"transient WAL index").expect("write live SHM");
    std::fs::copy(&live_wal, fixture.candidate_path("-wal")).expect("stage identical WAL");
    fixture.write_manifest(fixture.checksum_set(&["", "-wal"]));

    let report = fixture.promote();
    assert_eq!(report.status, "applied", "{report:#?}");
    assert!(report.sidecar_bundle_complete, "{report:#?}");
    assert_eq!(
        report.derived_assets_consistency_status,
        "live-derived-assets-reusable-same-archive-identity"
    );
    assert!(!report.derived_lexical_rebuild_required);
    assert!(!report.derived_semantic_rebuild_required);
    assert!(
        !live_shm.exists(),
        "stale SHM must be rebuilt for the promoted bundle"
    );
    assert_eq!(
        std::fs::read(Path::new(&report.backup_dir).join("live/agent_search.db-shm"))
            .expect("read preserved prior-live SHM"),
        b"transient WAL index"
    );
}

fn committed_marker(path: &Path) -> String {
    let connection = FrankenConnection::open_schema_only_with_wal_index_recovery(
        path.to_string_lossy().into_owned(),
    )
    .expect("read committed fixture DB/WAL with only derived SHM recovery");
    let marker = connection
        .query_row("SELECT marker FROM committed_rows WHERE id = 2")
        .expect("read row committed only in WAL")
        .get_typed::<String>(0)
        .expect("read committed marker");
    connection
        .close_without_checkpoint()
        .expect("close fixture reader without modifying DB or WAL");
    marker
}

#[test]
fn different_committed_wal_with_identical_base_db_requires_index_rebuild() {
    let fixture = CandidateFixture::new();
    let seed = fixture.data_dir.join("seed.db");
    let connection =
        FrankenConnection::open(seed.to_string_lossy().into_owned()).expect("open seed database");
    connection
        .execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA wal_autocheckpoint = 0;
             CREATE TABLE committed_rows (id INTEGER PRIMARY KEY, marker TEXT NOT NULL);
             INSERT INTO committed_rows VALUES (1, 'checkpointed');",
        )
        .expect("create checkpointed seed rows");
    connection
        .query("PRAGMA wal_checkpoint(TRUNCATE)")
        .expect("checkpoint seed DB");
    connection
        .close_without_checkpoint()
        .expect("close seed without another checkpoint");
    std::fs::copy(&seed, &fixture.db_path).expect("copy seed to live DB");
    std::fs::copy(&seed, fixture.candidate_path("")).expect("copy seed to candidate DB");

    for (path, marker) in [
        (fixture.db_path.clone(), "live WAL row"),
        (fixture.candidate_path(""), "restored WAL row"),
    ] {
        let writer =
            FrankenConnection::open(path.to_string_lossy().into_owned()).expect("open WAL writer");
        writer
            .execute("PRAGMA wal_autocheckpoint = 0")
            .expect("disable automatic checkpoints");
        writer
            .execute_with_params(
                "INSERT INTO committed_rows VALUES (2, ?1)",
                &[crate::franken_sync::SqliteValue::Text(marker.into())],
            )
            .expect("commit distinct row to WAL");
        writer
            .close_without_checkpoint()
            .expect("preserve uncheckpointed committed row");
    }
    assert_eq!(
        file_blake3_hex(&fixture.db_path).expect("hash live base DB"),
        file_blake3_hex(&fixture.candidate_path("")).expect("hash candidate base DB"),
        "the checkpointed DB bytes must be identical for this regression"
    );
    assert_ne!(
        file_blake3_hex(
            &doctor_sqlite_sidecar_path(&fixture.db_path, "-wal").expect("live WAL path"),
        )
        .expect("hash live WAL"),
        file_blake3_hex(&fixture.candidate_path("-wal")).expect("hash candidate WAL")
    );
    assert_eq!(committed_marker(&fixture.db_path), "live WAL row");
    assert_eq!(
        committed_marker(&fixture.candidate_path("")),
        "restored WAL row"
    );
    fixture.write_manifest(fixture.checksum_set(&["", "-wal"]));

    let report = fixture.promote();
    assert_eq!(report.status, "applied", "{report:#?}");
    assert!(report.derived_lexical_rebuild_required, "{report:#?}");
    assert!(report.derived_semantic_rebuild_required, "{report:#?}");
    assert_eq!(
        report.derived_assets_consistency_status,
        "live-derived-assets-stale-after-archive-promotion-rebuild-required"
    );
    assert_eq!(committed_marker(&fixture.db_path), "restored WAL row");
    assert_eq!(
        committed_marker(&Path::new(&report.backup_dir).join("live/agent_search.db")),
        "live WAL row",
        "the rollback backup must retain the former committed WAL generation"
    );
}
