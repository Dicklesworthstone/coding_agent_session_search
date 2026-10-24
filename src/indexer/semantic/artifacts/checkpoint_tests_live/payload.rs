//! Checkpoint cardinality must not authorize retiring the last usable vectors.
//! Every changed image is a fresh fixture, written with the native FSVI writer.

use super::*;
use crate::indexer::semantic::HASH_VECTOR_SPACE_REVISION;
use frankensearch::index::Quantization;

fn replace_payload(
    fixture: &Fixture,
    bad_position: usize,
    value: f32,
    extra: bool,
) -> Result<String> {
    let reader = VectorIndex::open_read_only(&fixture.checkpoint)?;
    let ids = (0..reader.record_count())
        .map(|row| reader.doc_id_at(row).map(str::to_owned))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(reader);
    let bad_id = if extra {
        "unacknowledged-extra-document".to_owned()
    } else {
        ids[bad_position].clone()
    };
    let mut writer = VectorIndex::create_with_revision(
        &fixture.checkpoint,
        "fnv1a-384",
        HASH_VECTOR_SPACE_REVISION,
        384,
        Quantization::F16,
    )?;
    for id in ids.iter().chain(extra.then_some(&bad_id)) {
        let mut vector = vec![0.0; 384];
        vector[0] = if id == &bad_id { value } else { 1.0 };
        writer.write_record(id, &vector)?;
    }
    writer.finish()?;
    let reader = VectorIndex::open_read_only(&fixture.checkpoint)?;
    // Locate by full ID: the native writer may reorder physical rows.
    let bad_row = (0..reader.record_count())
        .find(|&row| reader.doc_id_at(row).unwrap() == bad_id)
        .expect("invalid fixture payload must exist");
    assert!(!reader.is_vector_usable(bad_row));
    checkpoint_coverage::require_distinct_live(
        &reader,
        fixture.manifest.checkpoint.as_ref().unwrap().docs_embedded,
    )?;
    Ok(bad_id)
}

fn assert_checkpoint_payload_refused(
    fixture: &mut Fixture,
    data: &Path,
    approval: &BackfillArtifactReclaimPlan,
) -> Result<()> {
    let error = validate_reclaim_checkpoint(
        &fixture.checkpoint,
        fixture.manifest.checkpoint.as_ref().unwrap(),
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("unusable live vector"),
        "{error:#}"
    );
    // A corrupt resume cursor is not the typed ready-publication failure that
    // allows a new build to enter. It must preserve the existing refusal policy.
    assert!(!error.is::<publication::PublicationUnavailable>());
    fixture.assert_refused(data, approval)
}

fn assert_approved_without_repair(fixture: &Fixture, data: &Path) -> Result<()> {
    let before = fixture.snapshot(data)?;
    let wal = wal_path_for(&fixture.checkpoint);
    let wal_before = fs::read(&wal).ok();
    let query = HashEmbedder::default().embed_sync("compiler recovery document")?;
    let reader = VectorIndex::open_read_only(&fixture.checkpoint)?;
    let hits = reader.search_top_k(&query, 10, None)?;
    let approval = plan_backfill_artifacts(data)?;
    let report = apply_backfill_artifact_plan(data, &approval.plan_fingerprint)?;
    assert_eq!((report.removed_files, report.removed_directories), (1, 1));
    assert!(report.failed_paths.is_empty());
    for (path, bytes) in before {
        if fixture.fallbacks.contains(&path) {
            assert!(!path.exists());
        } else {
            assert_eq!(fs::read(&path)?, bytes, "modified {}", path.display());
        }
    }
    assert_eq!(
        fs::read(&wal).ok(),
        wal_before,
        "inspection must not repair or compact"
    );
    assert_eq!(reader.search_top_k(&query, 10, None)?, hits);
    assert_eq!(
        VectorIndex::open_read_only(&fixture.checkpoint)?.search_top_k(&query, 10, None)?,
        hits
    );
    Ok(())
}

#[test]
fn unusable_quantized_checkpoint_cannot_retire_usable_fallbacks() -> Result<()> {
    for tier in [TierKind::Fast, TierKind::Quality] {
        for value in [1.0e-8_f32, 70_000.0_f32] {
            for bad_position in [0, 2] {
                let temp = tempfile::tempdir()?;
                let data = temp.path();
                let mut fixture = Fixture::new(data, tier)?;
                let approval = plan_backfill_artifacts(data)?;
                replace_payload(&fixture, bad_position, value, false)?;
                // Both finite f32 inputs pass the old cardinality-only guard:
                // underflow loses all signal in F16; overflow persists infinity.
                assert_checkpoint_payload_refused(&mut fixture, data, &approval)?;
            }
        }
    }
    Ok(())
}

#[test]
fn unusable_live_rows_ahead_of_the_saved_checkpoint_also_preserve_fallbacks() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let mut fixture = Fixture::new(data, TierKind::Quality)?;
    let approval = plan_backfill_artifacts(data)?;
    replace_payload(&fixture, 0, 1.0e-8, true)?;
    let reader = VectorIndex::open_read_only(&fixture.checkpoint)?;
    assert_eq!(reader.record_count(), 4);
    assert_eq!(
        fixture.manifest.checkpoint.as_ref().unwrap().docs_embedded,
        3
    );
    drop(reader);
    assert_checkpoint_payload_refused(&mut fixture, data, &approval)
}

#[test]
fn checkpoint_wal_replacement_shadows_unusable_untombstoned_main_data() -> Result<()> {
    for tier in [TierKind::Fast, TierKind::Quality] {
        let temp = tempfile::tempdir()?;
        let data = temp.path();
        let fixture = Fixture::new(data, tier)?;
        let bad_id = replace_payload(&fixture, 0, 1.0e-8, false)?;
        let untombstoned = fs::read(&fixture.checkpoint)?;
        let mut writer = VectorIndex::open_writer(&fixture.checkpoint)?;
        let mut replacement = vec![0.0; 384];
        replacement[0] = 1.0;
        writer.append(&bad_id, &replacement)?;
        drop(writer);
        // Native WAL plus prior main models a crash after durable append but
        // before tombstoning. Do not construct or patch a synthetic WAL format.
        fs::write(&fixture.checkpoint, untombstoned)?;
        let reader = VectorIndex::open_read_only(&fixture.checkpoint)?;
        let row = (0..reader.record_count())
            .find(|&row| reader.doc_id_at(row).unwrap() == bad_id)
            .unwrap();
        assert!(!reader.is_deleted(row));
        assert!(!reader.is_vector_usable(row));
        assert_eq!(reader.wal_record_count(), 1);
        assert_eq!(reader.wal_records().next().unwrap().0, bad_id);
        drop(reader);
        assert_approved_without_repair(&fixture, data)?;
    }
    Ok(())
}

#[test]
fn tombstoned_unusable_checkpoint_rows_do_not_block_live_coverage() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let mut fixture = Fixture::new(data, TierKind::Quality)?;
    let bad_id = replace_payload(&fixture, 2, 70_000.0, false)?;
    let mut writer = VectorIndex::open_writer(&fixture.checkpoint)?;
    assert!(writer.soft_delete(&bad_id)?);
    drop(writer);
    fixture.manifest.checkpoint.as_mut().unwrap().docs_embedded = 2;
    fixture.manifest.save(data)?;
    assert_approved_without_repair(&fixture, data)
}

#[test]
fn explicit_checkpoint_restoration_allows_recovery_and_same_cursor_resume() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let mut fixture = Fixture::new(data, TierKind::Quality)?;
    let valid_checkpoint = fs::read(&fixture.checkpoint)?;
    let input = fixture.manifest.clone();
    let approval = plan_backfill_artifacts(data)?;
    replace_payload(&fixture, 1, 1.0e-8, false)?;
    assert_checkpoint_payload_refused(&mut fixture, data, &approval)?;
    assert_eq!(fixture.manifest, input);
    // Only the explicit fixture restoration supplies valid data. Recovery
    // neither chooses a fallback nor changes the acknowledged cursor itself.
    fs::write(&fixture.checkpoint, valid_checkpoint)?;
    assert_approved_without_repair(&fixture, data)?;
    let outcome = fixture.indexer.run_backfill_batch(
        &[EmbeddingInput::new(4, "fourth compiler recovery document")],
        data,
        &mut fixture.manifest,
        SemanticBackfillBatchPlan {
            tier: TierKind::Quality,
            db_fingerprint: "content-v1:resume".into(),
            model_revision: "hash".into(),
            total_conversations: 4,
            conversations_in_batch: 1,
            last_offset: 4,
            cursor_exhausted: true,
        },
    )?;
    assert!(outcome.published && fixture.manifest.checkpoint.is_none());
    let published = VectorIndex::open_read_only(&outcome.index_path)?;
    assert_eq!(published.record_count(), 4);
    for row in 0..published.record_count() {
        assert!(published.is_vector_usable(row));
    }
    Ok(())
}
