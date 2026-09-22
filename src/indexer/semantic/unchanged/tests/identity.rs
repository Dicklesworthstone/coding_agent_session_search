use super::*;
use std::io::{Seek, SeekFrom, Write};

#[test]
fn refreshed_receipt_uses_the_existing_engine_cache_and_rejects_later_wal_edits() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let (storage, mut manifest, plan, _) = setup(data)?;
    fs::write(cache_path(data), b"{truncated")?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let first = indexer.run_backfill_from_storage(&storage, data, &mut manifest, plan.clone())?;
    assert!(first.unchanged);
    let receipt = fs::read(cache_path(data))?;
    assert_eq!(
        indexer.completed_backfill_fingerprint(
            &storage,
            data,
            &manifest,
            plan.tier,
            &plan.model_revision
        )?,
        Some(plan.db_fingerprint.clone())
    );
    let inner = engine::SemanticIndexer::new("hash", None)?;
    assert!(
        try_retain_completed(
            &inner,
            &storage,
            data,
            &manifest,
            &plan,
            &SemanticProgressSink::disabled()
        )?
        .is_none(),
        "a cache hit must skip the new full proof"
    );
    let second = indexer.run_backfill_from_storage(&storage, data, &mut manifest, plan.clone())?;
    assert!(second.unchanged);
    assert_eq!(
        fs::read(cache_path(data))?,
        receipt,
        "cache hits must not rewrite receipts"
    );
    storage
        .raw()
        .execute("UPDATE messages SET content = 'new content after cached proof' WHERE id = 1")?;
    assert_eq!(
        crate::indexer::lexical_storage_fingerprint_for_storage(&storage)?,
        plan.db_fingerprint
    );
    assert!(
        indexer
            .completed_backfill_fingerprint(
                &storage,
                data,
                &manifest,
                plan.tier,
                &plan.model_revision
            )?
            .is_none()
    );
    assert!(
        prove_unchanged(
            &inner,
            &storage,
            data,
            &manifest,
            &plan,
            &SemanticProgressSink::disabled(),
            || Ok(())
        )?
        .is_none()
    );
    assert_eq!(
        fs::read(cache_path(data))?,
        receipt,
        "a failed proof must not mint a new receipt"
    );
    Ok(())
}

#[test]
fn failed_receipt_write_does_not_fail_a_no_op_or_damage_serving_files() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let (storage, mut manifest, plan, _) = setup(data)?;
    fs::rename(cache_path(data), data.join("retained-original-cache.json"))?;
    fs::create_dir(cache_path(data))?;
    fs::write(cache_path(data).join("keep"), b"not a writable cache")?;
    let before = snapshot(data)?;
    let ledger = manifest.clone();
    let outcome = SemanticIndexer::new("hash", None)?.run_backfill_from_storage(
        &storage,
        data,
        &mut manifest,
        plan,
    )?;
    assert!(outcome.unchanged && outcome.published);
    assert_eq!(manifest, ledger);
    assert_eq!(snapshot(data)?, before);
    assert_eq!(
        fs::read(cache_path(data).join("keep"))?,
        b"not a writable cache"
    );
    Ok(())
}

#[test]
fn pinned_sql_snapshot_and_replaced_archive_path_cannot_borrow_current_file_stamps() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let (storage, manifest, plan, _) = setup(data)?;
    let inner = engine::SemanticIndexer::new("hash", None)?;
    storage.raw().execute("BEGIN")?;
    storage
        .raw()
        .query("SELECT content FROM messages WHERE id = 1")?;
    let before = snapshot(data)?;
    assert!(ArchiveStamp::capture(&storage)?.is_none());
    assert!(
        prove_unchanged(
            &inner,
            &storage,
            data,
            &manifest,
            &plan,
            &SemanticProgressSink::disabled(),
            || Ok(())
        )?
        .is_none()
    );
    assert_eq!(snapshot(data)?, before);
    storage.raw().execute("ROLLBACK")?;
    assert!(ArchiveStamp::capture(&storage)?.is_some());

    let replacement = data.join("replacement.db");
    drop(FrankenStorage::open(&replacement)?);
    let path = data.join("agent_search.db");
    fs::rename(&path, data.join("retained-original.db"))?;
    fs::rename(&replacement, &path)?;
    assert!(ArchiveStamp::capture(&storage)?.is_none());
    assert!(
        prove_unchanged(
            &inner,
            &storage,
            data,
            &manifest,
            &plan,
            &SemanticProgressSink::disabled(),
            || Ok(())
        )?
        .is_none()
    );
    assert_eq!(snapshot(data)?, before);
    Ok(())
}

#[test]
fn same_length_vector_edit_with_restored_mtime_invalidates_proof_and_does_not_refresh_cache()
-> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let (storage, manifest, plan, live) = setup(data)?;
    fs::write(cache_path(data), b"retained invalid receipt")?;
    let ledger = fs::read(SemanticManifest::path(data))?;
    let modified = fs::metadata(&live)?.modified()?;
    let inner = engine::SemanticIndexer::new("hash", None)?;
    let error = prove_unchanged(
        &inner,
        &storage,
        data,
        &manifest,
        &plan,
        &SemanticProgressSink::disabled(),
        || {
            let old = fs::read(&live)?;
            let mut file = File::options().write(true).open(&live)?;
            file.seek(SeekFrom::End(-1))?;
            file.write_all(&[old[old.len() - 1] ^ 1])?;
            file.sync_all()?;
            file.set_modified(modified)?;
            Ok(())
        },
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("changed during unchanged proof"));
    assert_eq!(fs::read(SemanticManifest::path(data))?, ledger);
    assert_eq!(fs::read(cache_path(data))?, b"retained invalid receipt");
    Ok(())
}

#[test]
fn foreign_revision_or_tombstoned_vector_image_is_not_a_completed_no_op() -> Result<()> {
    for tombstone in [false, true] {
        let temp = tempfile::tempdir()?;
        let data = temp.path();
        let (storage, manifest, plan, live) = setup(data)?;
        let reader = VectorIndex::open_read_only(&live)?;
        let records: Vec<_> = (0..reader.record_count())
            .map(|i| Ok((reader.doc_id_at(i)?.to_owned(), reader.vector_at_f32(i)?)))
            .collect::<Result<_>>()?;
        drop(reader);
        let expected = engine::expected_vector_space_revision("fnv1a-384").unwrap();
        let revision = if tombstone {
            expected.to_owned()
        } else {
            "x".repeat(expected.len())
        };
        let mut writer = VectorIndex::create_with_revision(
            &live,
            "fnv1a-384",
            &revision,
            384,
            Quantization::F16,
        )?;
        for (i, (id, vector)) in records.iter().enumerate() {
            if tombstone && i == 0 {
                writer.write_tombstone_record(id, vector)?;
            } else {
                writer.write_record(id, vector)?;
            }
        }
        writer.finish()?;
        assert_eq!(
            fs::metadata(&live)?.len(),
            manifest.quality_tier.as_ref().unwrap().size_bytes
        );
        let before = snapshot(data)?;
        let inner = engine::SemanticIndexer::new("hash", None)?;
        assert!(
            prove_unchanged(
                &inner,
                &storage,
                data,
                &manifest,
                &plan,
                &SemanticProgressSink::disabled(),
                || Ok(())
            )?
            .is_none()
        );
        assert_eq!(snapshot(data)?, before);
    }
    Ok(())
}
