//! Replayed physical records must not impersonate distinct acknowledged docs.
use super::*;
use std::collections::HashSet;

/// Produce the native crash boundary after WAL durability but before the main
/// tombstone update: append using the real writer, then restore ONLY this
/// temporary fixture's prior main image while retaining the generated WAL.
fn append_before_tombstone(path: &Path, repeats: usize) -> Result<()> {
    let before = fs::read(path)?;
    let mut writer = VectorIndex::open(path)?;
    let row = (0..writer.record_count())
        .find(|&row| !writer.is_deleted(row))
        .expect("fixture needs one surviving main document");
    let id = writer.doc_id_at(row)?.to_owned();
    let vector = writer.vector_at_f32(row)?;
    for _ in 0..repeats {
        writer.append(&id, &vector)?;
    }
    drop(writer);
    fs::write(path, before)?;
    let reader = VectorIndex::open_read_only(path)?;
    assert_eq!(
        reader.wal_record_count(),
        1,
        "native WAL replay deduplicates updates"
    );
    assert!(reader.wal_records().all(|(wal_id, _)| wal_id == id));
    assert!(
        (0..reader.record_count())
            .any(|row| !reader.is_deleted(row) && reader.doc_id_at(row).unwrap() == id),
        "fixture must retain an actual main/WAL overlap"
    );
    Ok(())
}

fn distinct_ids(index: &VectorIndex) -> Result<HashSet<String>> {
    let mut ids = HashSet::new();
    for row in 0..index.record_count() {
        if !index.is_deleted(row) {
            ids.insert(index.doc_id_at(row)?.to_owned());
        }
    }
    ids.extend(index.wal_records().map(|(id, _)| id.to_owned()));
    Ok(ids)
}

#[test]
fn overlap_cannot_cover_another_missing_acknowledged_document() -> Result<()> {
    for tier in [TierKind::Fast, TierKind::Quality] {
        for repeats in [1, 3] {
            let temp = tempfile::tempdir()?;
            let data = temp.path();
            let mut fixture = Fixture::new(data, tier)?;
            let approval = plan_backfill_artifacts(data)?;
            fixture.tombstone(false)?; // Lose one of three acknowledged documents.
            append_before_tombstone(&fixture.checkpoint, repeats)?;
            let index = VectorIndex::open_read_only(&fixture.checkpoint)?;
            assert_eq!(
                index.record_count() - index.tombstone_count() + index.wal_record_count(),
                3
            );
            assert_eq!(distinct_ids(&index)?.len(), 2);
            drop(index);
            let error = validate_reclaim_checkpoint(
                &fixture.checkpoint,
                fixture.manifest.checkpoint.as_ref().unwrap(),
            )
            .unwrap_err();
            assert!(format!("{error:#}").contains("2 distinct live documents"));
            // Covers startup, post-success deferral, explicit recovery, preview,
            // and approved apply, with byte-for-byte and live-search assertions.
            fixture.assert_refused(data, &approval)?;
        }
    }
    Ok(())
}

#[test]
fn healthy_overlap_and_wal_replacement_are_not_blanket_rejected() -> Result<()> {
    for restore_missing_in_wal in [false, true] {
        let temp = tempfile::tempdir()?;
        let data = temp.path();
        let fixture = Fixture::new(data, TierKind::Quality)?;
        let original = VectorIndex::open_read_only(&fixture.checkpoint)?;
        let first_id = original.doc_id_at(0)?.to_owned();
        let first_vector = original.vector_at_f32(0)?;
        drop(original);
        if restore_missing_in_wal {
            fixture.tombstone(false)?;
        }
        append_before_tombstone(&fixture.checkpoint, 1)?;
        if restore_missing_in_wal {
            let mut writer = VectorIndex::open(&fixture.checkpoint)?;
            writer.append(&first_id, &first_vector)?;
        }
        let index = VectorIndex::open_read_only(&fixture.checkpoint)?;
        assert_eq!(distinct_ids(&index)?.len(), 3);
        let query = HashEmbedder::default().embed_sync("compiler recovery document")?;
        let hits = index.search_top_k(&query, 10, None)?;
        let before = fixture.snapshot(data)?;
        let wal = wal_path_for(&fixture.checkpoint);
        let wal_before = fs::read(&wal)?;
        let approved = plan_backfill_artifacts(data)?;
        let report = apply_backfill_artifact_plan(data, &approved.plan_fingerprint)?;
        assert_eq!((report.removed_files, report.removed_directories), (1, 1));
        assert!(report.failed_paths.is_empty());
        for (path, bytes) in before {
            if !fixture.fallbacks.contains(&path) {
                assert_eq!(fs::read(path)?, bytes);
            }
        }
        assert_eq!(
            fs::read(&wal)?,
            wal_before,
            "reclamation must not compact or repair"
        );
        assert_eq!(index.search_top_k(&query, 10, None)?, hits);
        assert_eq!(
            VectorIndex::open_read_only(&fixture.checkpoint)?.search_top_k(&query, 10, None)?,
            hits
        );
    }
    Ok(())
}

#[test]
fn duplicate_main_slots_cannot_authorize_retiring_fallbacks() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let mut fixture = Fixture::new(data, TierKind::Quality)?;
    let approval = plan_backfill_artifacts(data)?;
    let reader = VectorIndex::open_read_only(&fixture.checkpoint)?;
    let rows: Vec<_> = [0, 0, 1]
        .into_iter()
        .map(|row| {
            Ok((
                reader.doc_id_at(row)?.to_owned(),
                reader.vector_at_f32(row)?,
            ))
        })
        .collect::<Result<_>>()?;
    drop(reader);
    let mut writer = VectorIndex::create_with_revision(
        &fixture.checkpoint,
        "fnv1a-384",
        crate::indexer::semantic::expected_vector_space_revision("fnv1a-384").unwrap(),
        384,
        frankensearch::index::Quantization::F16,
    )?;
    for (id, vector) in rows {
        writer.write_record(&id, &vector)?;
    }
    writer.finish()?;
    // Native reader rejection is also safe. If legacy admission accepts the
    // image, it has enough physical slots but not enough distinct documents.
    if let Ok(reader) = VectorIndex::open_read_only(&fixture.checkpoint) {
        assert_eq!(reader.record_count(), 3);
        assert_eq!(distinct_ids(&reader)?.len(), 2);
    }
    fixture.assert_refused(data, &approval)?;
    Ok(())
}
