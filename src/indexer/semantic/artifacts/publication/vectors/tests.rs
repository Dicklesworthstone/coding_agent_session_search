//! Real FSVI files exercise payload admission independently of row cardinality.
//! Every replacement and recovery operation is confined to a fresh tempdir.

use super::super::{PublicationUnavailable, validate_with};
use super::*;
use crate::indexer::semantic::{
    BackfillArtifactReclaimPlan, EmbeddingInput, HASH_VECTOR_SPACE_REVISION,
    SemanticBackfillBatchPlan, SemanticIndexer, apply_backfill_artifact_plan,
    plan_backfill_artifacts, reclaim_backfill_artifacts,
};
use crate::search::semantic_manifest::{SemanticManifest, TierKind};
use frankensearch::index::{Quantization, wal_path_for};
use std::fs;
use std::path::{Path, PathBuf};

fn batch(tier: TierKind, fingerprint: &str) -> SemanticBackfillBatchPlan {
    SemanticBackfillBatchPlan {
        tier,
        db_fingerprint: fingerprint.into(),
        model_revision: "hash".into(),
        total_conversations: 3,
        conversations_in_batch: 3,
        last_offset: 3,
        cursor_exhausted: true,
    }
}

fn inputs() -> Vec<EmbeddingInput> {
    (1..=3)
        .map(|id| EmbeddingInput::new(id, format!("durable vector recovery payload {id}")))
        .collect()
}

struct Fixture {
    manifest: SemanticManifest,
    live: PathBuf,
    staging: PathBuf,
    reuse: PathBuf,
    ids: Vec<String>,
}

impl Fixture {
    fn new(data: &Path, tier: TierKind) -> Result<Self> {
        let indexer = SemanticIndexer::new("hash", None)?;
        let mut manifest = SemanticManifest::default();
        let live = indexer
            .run_backfill_batch(
                &inputs(),
                data,
                &mut manifest,
                batch(tier, "content-v1:signal"),
            )?
            .index_path;
        let reader = VectorIndex::open_read_only(&live)?;
        let ids = (0..reader.record_count())
            .map(|row| Ok(reader.doc_id_at(row)?.to_owned()))
            .collect::<Result<Vec<_>>>()?;
        let parent = live.parent().unwrap();
        let staging = parent.join(".staging-quality-fnv1a-384-deadbeef.fsvi");
        fs::copy(&live, &staging)?;
        let reuse = parent.join(".backfill-reuse-Usability123");
        fs::create_dir(&reuse)?;
        fs::copy(&live, reuse.join("candidate.fsvi"))?;
        Ok(Self {
            manifest,
            live,
            staging,
            reuse,
            ids,
        })
    }

    fn replace_payload(&self, bad_row: usize, value: f32, append_extra: bool) -> Result<()> {
        let mut writer = VectorIndex::create_with_revision(
            &self.live,
            "fnv1a-384",
            HASH_VECTOR_SPACE_REVISION,
            384,
            Quantization::F16,
        )?;
        for (row, id) in self.ids.iter().enumerate() {
            let mut vector = vec![0.0; 384];
            vector[0] = if row == bad_row { value } else { 1.0 };
            writer.write_record(id, &vector)?;
        }
        if append_extra {
            let mut vector = vec![0.0; 384];
            vector[0] = value;
            writer.write_record("extra-live-document", &vector)?;
        }
        writer.finish()?;
        Ok(())
    }

    fn assert_refused(&self, data: &Path, approved: &BackfillArtifactReclaimPlan) -> Result<()> {
        let paths = [
            self.live.clone(),
            self.staging.clone(),
            self.reuse.join("candidate.fsvi"),
            SemanticManifest::path(data),
        ];
        let before = paths
            .iter()
            .map(fs::read)
            .collect::<std::io::Result<Vec<_>>>()?;
        let wal_before = optional_bytes(&wal_path_for(&self.live))?;
        let mut pins = 0;
        let error = validate_with(data, &self.manifest, |_, _, _, _| {
            pins += 1;
            Ok(())
        })
        .unwrap_err();
        assert!(error.is::<PublicationUnavailable>(), "{error:#}");
        assert!(
            format!("{error:#}").contains("unusable live vector"),
            "{error:#}"
        );
        assert_eq!(
            pins, 0,
            "invalid payload must fail before durability admission"
        );
        for outcome in [
            plan_backfill_artifacts(data).map(|_| ()),
            reclaim_backfill_artifacts(data).map(|_| ()),
            apply_backfill_artifact_plan(data, &approved.plan_fingerprint).map(|_| ()),
        ] {
            let error = outcome.unwrap_err();
            assert!(error.is::<PublicationUnavailable>(), "{error:#}");
        }
        let after = paths
            .iter()
            .map(fs::read)
            .collect::<std::io::Result<Vec<_>>>()?;
        assert_eq!(after, before);
        assert_eq!(optional_bytes(&wal_path_for(&self.live))?, wal_before);
        assert!(self.reuse.is_dir());
        let retained = VectorIndex::open_read_only(&self.staging)?;
        assert_eq!(retained.record_count(), 3);
        for row in 0..retained.record_count() {
            assert!(
                retained.is_vector_usable(row),
                "the fallback must really be usable"
            );
        }
        Ok(())
    }
}

fn optional_bytes(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[test]
fn finite_f32_that_loses_signal_in_f16_cannot_authorize_fallback_reclamation() -> Result<()> {
    for tier in [TierKind::Fast, TierKind::Quality] {
        for value in [1.0e-8_f32, 70_000.0_f32] {
            for bad_row in [0, 2] {
                let temp = tempfile::tempdir()?;
                let data = temp.path();
                let fixture = Fixture::new(data, tier)?;
                let approval = plan_backfill_artifacts(data)?;
                fixture.replace_payload(bad_row, value, false)?;
                let reader = VectorIndex::open_read_only(&fixture.live)?;
                assert_eq!(reader.record_count(), 3);
                assert!(!reader.is_vector_usable(bad_row));
                super::super::super::checkpoint_coverage::require_distinct_live(&reader, 3)?;
                drop(reader);
                fixture.assert_refused(data, &approval)?;
            }
        }
    }
    Ok(())
}

#[test]
fn admission_checks_live_payload_beyond_the_recorded_count() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let fixture = Fixture::new(data, TierKind::Quality)?;
    let approval = plan_backfill_artifacts(data)?;
    fixture.replace_payload(usize::MAX, 1.0e-8, true)?;
    let reader = VectorIndex::open_read_only(&fixture.live)?;
    assert_eq!(reader.record_count(), 4);
    // Locate by identity: native writers may reorder physical rows.
    let extra_row = (0..reader.record_count())
        .find(|&row| reader.doc_id_at(row).unwrap() == "extra-live-document")
        .expect("the extra live document must be in the persisted image");
    assert!(!reader.is_vector_usable(extra_row));
    super::super::super::checkpoint_coverage::require_distinct_live(&reader, 3)?;
    drop(reader);
    fixture.assert_refused(data, &approval)
}

#[test]
fn deleted_unusable_rows_do_not_block_recovery_of_a_usable_publication() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let mut fixture = Fixture::new(data, TierKind::Quality)?;
    fixture.replace_payload(0, 1.0e-8, false)?;
    let mut writer = VectorIndex::open_writer(&fixture.live)?;
    assert!(writer.soft_delete(&fixture.ids[0])?);
    drop(writer);
    fixture.manifest.quality_tier.as_mut().unwrap().doc_count = 2;
    fixture.manifest.save(data)?;
    let reader = VectorIndex::open_read_only(&fixture.live)?;
    assert!(reader.is_deleted(0));
    require_usable_live_vectors(&reader)?;
    drop(reader);
    let before = fs::read(&fixture.live)?;
    let approved = plan_backfill_artifacts(data)?;
    let report = apply_backfill_artifact_plan(data, &approved.plan_fingerprint)?;
    assert_eq!((report.removed_files, report.removed_directories), (1, 1));
    assert!(report.failed_paths.is_empty());
    assert_eq!(fs::read(&fixture.live)?, before);
    Ok(())
}

#[test]
fn acknowledged_wal_replacement_overrides_unusable_but_untombstoned_main() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let fixture = Fixture::new(data, TierKind::Quality)?;
    fixture.replace_payload(0, 1.0e-8, false)?;
    let untombstoned = fs::read(&fixture.live)?;
    let mut writer = VectorIndex::open_writer(&fixture.live)?;
    let mut vector = vec![0.0; 384];
    vector[0] = 1.0;
    writer.append(&fixture.ids[0], &vector)?;
    drop(writer);
    // Reproduce interruption after WAL durability but before main tombstoning.
    // The actual native log is preserved, not replaced by fabricated WAL bytes.
    fs::write(&fixture.live, &untombstoned)?;
    let reader = VectorIndex::open_read_only(&fixture.live)?;
    assert!(!reader.is_deleted(0));
    assert!(!reader.is_vector_usable(0));
    assert_eq!(reader.wal_record_count(), 1);
    assert_eq!(reader.wal_records().next().unwrap().0, fixture.ids[0]);
    require_usable_live_vectors(&reader)?;
    drop(reader);
    let wal = wal_path_for(&fixture.live);
    let wal_before = fs::read(&wal)?;
    let approved = plan_backfill_artifacts(data)?;
    let report = apply_backfill_artifact_plan(data, &approved.plan_fingerprint)?;
    assert_eq!((report.removed_files, report.removed_directories), (1, 1));
    assert!(report.failed_paths.is_empty());
    assert_eq!(fs::read(&fixture.live)?, untombstoned);
    assert_eq!(fs::read(&wal)?, wal_before);
    Ok(())
}

#[test]
fn explicit_backfill_can_replace_bad_payload_before_reclaiming_fallbacks() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let mut fixture = Fixture::new(data, TierKind::Quality)?;
    let approved = plan_backfill_artifacts(data)?;
    fixture.replace_payload(0, 70_000.0, false)?;
    fixture.assert_refused(data, &approved)?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let outcome = indexer.run_backfill_batch(
        &inputs(),
        data,
        &mut fixture.manifest,
        batch(TierKind::Quality, "content-v1:repaired"),
    )?;
    assert!(outcome.published);
    assert!(!fixture.staging.exists() && !fixture.reuse.exists());
    let reader = VectorIndex::open_read_only(&outcome.index_path)?;
    assert_eq!(reader.record_count(), 3);
    require_usable_live_vectors(&reader)?;
    Ok(())
}

#[test]
fn wal_payload_contract_rejects_zero_nonfinite_overflow_and_wrong_dimensions() {
    assert!(usable_wal_vector(&[1.0, 0.0], 2));
    for values in [
        vec![],
        vec![1.0],
        vec![0.0, 0.0],
        vec![f32::NAN, 1.0],
        vec![f32::INFINITY, 0.0],
        vec![f32::NEG_INFINITY, 0.0],
        vec![f32::MAX, 1.0],
    ] {
        assert!(!usable_wal_vector(&values, 2), "{values:?}");
    }
}
