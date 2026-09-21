//! A durable path is not proof that the replacement checkpoint is readable.
//! Real hash-produced FSVI files exercise GC through its public entry points.

use super::*;
use crate::indexer::semantic::{EmbeddingInput, SemanticBackfillBatchPlan, SemanticIndexer};
use crate::search::embedder::Embedder;
use crate::search::hash_embedder::HashEmbedder;
use crate::search::semantic_manifest::TierKind;
use frankensearch::index::{Quantization, VectorIndex};

fn plan(fingerprint: &str, complete: bool) -> SemanticBackfillBatchPlan {
    SemanticBackfillBatchPlan {
        tier: TierKind::Quality,
        db_fingerprint: fingerprint.into(),
        model_revision: "hash".into(),
        total_conversations: 3,
        conversations_in_batch: 1,
        last_offset: 1,
        cursor_exhausted: complete,
    }
}

fn rows(id: u64) -> Vec<EmbeddingInput> {
    vec![EmbeddingInput::new(id, format!("compiler recovery evidence {id}"))]
}

fn setup(data: &Path) -> Result<(SemanticIndexer, SemanticManifest, PathBuf, PathBuf)> {
    let indexer = SemanticIndexer::new("hash", None)?;
    let mut manifest = SemanticManifest::default();
    let live = indexer.run_backfill_batch(
        &rows(10), data, &mut manifest, plan("content-v1:published", true),
    )?.index_path;
    let checkpoint = indexer.run_backfill_batch(
        &rows(1), data, &mut manifest, plan("content-v1:checkpoint", false),
    )?.index_path;
    Ok((indexer, manifest, live, checkpoint))
}

fn fallback_files(data: &Path, checkpoint: &Path) -> Result<Vec<(PathBuf, Vec<u8>)>> {
    let root = data.join(VECTOR_INDEX_DIR);
    let fallback = root.join(".staging-quality-fnv1a-384-deadbeef.fsvi");
    fs::copy(checkpoint, &fallback)?;
    let wal = wal_path_for(&fallback);
    fs::write(&wal, b"retained prior WAL")?;
    let reuse = root.join(".backfill-reuse-Interrupted123");
    fs::create_dir(&reuse)?;
    let candidate = reuse.join("candidate.fsvi");
    fs::copy(checkpoint, &candidate)?;
    [fallback, wal, candidate].into_iter().map(|path| {
        let bytes = fs::read(&path)?;
        Ok((path, bytes))
    }).collect()
}

fn assert_preserved(files: &[(PathBuf, Vec<u8>)]) -> Result<()> {
    for (path, bytes) in files {
        assert_eq!(&fs::read(path)?, bytes, "changed {}", path.display());
    }
    Ok(())
}

#[test]
fn truncated_checkpoint_preserves_fallbacks_live_bytes_and_search_on_every_entry_point() -> Result<()> {
    for len in [0, 1, 16] {
        let temp = tempfile::tempdir()?;
        let data = temp.path();
        let (indexer, mut manifest, live, checkpoint) = setup(data)?;
        let files = fallback_files(data, &checkpoint)?;
        let before_manifest = fs::read(SemanticManifest::path(data))?;
        let before_live = fs::read(&live)?;
        let saved = fs::read(&checkpoint)?;
        let reader = VectorIndex::open_read_only(&live)?;
        let query = HashEmbedder::default().embed_sync("compiler recovery evidence 10")?;
        let hits = format!("{:?}", reader.search_top_k(&query, 3, None)?);
        let approval = plan_backfill_artifacts(data)?;
        fs::write(&checkpoint, &saved[..len])?;

        let error = reclaim_backfill_artifacts(data).unwrap_err();
        assert!(format!("{error:#}").contains("retaining fallback artifacts"));
        assert!(plan_backfill_artifacts(data).is_err());
        assert!(apply_backfill_artifact_plan(data, &approval.plan_fingerprint).is_err());
        // This public call must stop before embedding or creating a replacement.
        assert!(indexer.run_backfill_batch(
            &rows(2), data, &mut manifest, plan("content-v1:next", false),
        ).is_err());

        assert_preserved(&files)?;
        assert_eq!(fs::read(&checkpoint)?, &saved[..len]);
        assert_eq!(fs::read(SemanticManifest::path(data))?, before_manifest);
        assert_eq!(fs::read(&live)?, before_live);
        assert_eq!(hits, format!("{:?}", reader.search_top_k(&query, 3, None)?));
        let reopened = VectorIndex::open_read_only(&live)?;
        assert_eq!(hits, format!("{:?}", reopened.search_top_k(&query, 3, None)?));
    }
    Ok(())
}

#[test]
fn valid_fsvi_from_a_different_producer_cannot_authorize_reclamation() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let (_, _, live, checkpoint) = setup(data)?;
    let files = fallback_files(data, &checkpoint)?;
    let before_live = fs::read(&live)?;
    let before_manifest = fs::read(SemanticManifest::path(data))?;
    let foreign = data.join("foreign.fsvi");
    let mut writer = VectorIndex::create_with_revision(
        &foreign, "other-producer", "test-v1", 384, Quantization::F16,
    )?;
    writer.write_record("foreign-document", &[0.25_f32; 384])?;
    writer.finish()?;
    fs::copy(&foreign, &checkpoint)?;
    assert_eq!(VectorIndex::open_read_only(&checkpoint)?.embedder_id(), "other-producer");

    let error = reclaim_backfill_artifacts(data).unwrap_err();
    assert!(format!("{error:#}").contains("expected fnv1a-384"));
    assert!(plan_backfill_artifacts(data).is_err());
    assert_preserved(&files)?;
    assert_eq!(fs::read(&live)?, before_live);
    assert_eq!(fs::read(SemanticManifest::path(data))?, before_manifest);
    assert_eq!(fs::read(&foreign)?, fs::read(&checkpoint)?);
    Ok(())
}

#[test]
fn restoring_checkpoint_allows_a_new_approval_without_rewriting_live_data() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let (_, _, live, checkpoint) = setup(data)?;
    let files = fallback_files(data, &checkpoint)?;
    let saved = fs::read(&checkpoint)?;
    let before_live = fs::read(&live)?;
    let before_manifest = fs::read(SemanticManifest::path(data))?;
    fs::write(&checkpoint, b"truncated")?;
    assert!(reclaim_backfill_artifacts(data).is_err());
    assert_preserved(&files)?;
    // The fixture restores its known-good checkpoint. GC never chooses an
    // orphan as a replacement or invents a new checkpoint on the user's behalf.
    fs::write(&checkpoint, &saved)?;
    let approval = plan_backfill_artifacts(data)?;
    assert_eq!((approval.reclaimable_files, approval.reclaimable_directories), (2, 1));
    let report = apply_backfill_artifact_plan(data, &approval.plan_fingerprint)?;
    assert_eq!((report.removed_files, report.removed_directories), (2, 1));
    assert!(report.failed_paths.is_empty());
    assert_eq!(report.reclaimed_bytes, approval.reclaimable_bytes);
    for (path, _) in files { assert!(!path.exists()); }
    assert_eq!(fs::read(&live)?, before_live);
    assert_eq!(fs::read(&checkpoint)?, saved);
    assert_eq!(fs::read(SemanticManifest::path(data))?, before_manifest);
    assert_eq!(VectorIndex::open_read_only(&checkpoint)?.record_count(), 1);
    assert!(plan_backfill_artifacts(data)?.candidates.is_empty());
    Ok(())
}

fn setup_wal(data: &Path) -> Result<(PathBuf, PathBuf, Vec<u8>)> {
    let (indexer, mut manifest, live, _) = setup(data)?;
    // Keep one appended record below the default 10% compaction ratio, so
    // this verifies a WAL-backed checkpoint rather than an already compacted one.
    let base: Vec<_> = (1..=128).flat_map(rows).collect();
    let checkpoint = indexer.run_backfill_batch(
        &base, data, &mut manifest, plan("content-v1:wal-checkpoint", false),
    )?.index_path;
    let before_append = fs::read(SemanticManifest::path(data))?;
    indexer.run_backfill_batch(
        &rows(129), data, &mut manifest, plan("content-v1:wal-checkpoint", false),
    )?;
    assert_eq!(manifest.checkpoint.as_ref().unwrap().docs_embedded, 129);
    Ok((live, checkpoint, before_append))
}

#[test]
fn healthy_checkpoint_with_appended_wal_survives_preview_and_apply_byte_for_byte() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let (live, checkpoint, _) = setup_wal(data)?;
    let wal = wal_path_for(&checkpoint);
    assert!(wal.is_file(), "fixture must exercise a real appended FSVI WAL");
    let before = [(checkpoint.clone(), fs::read(&checkpoint)?), (wal.clone(), fs::read(&wal)?),
        (live.clone(), fs::read(&live)?),
        (SemanticManifest::path(data), fs::read(SemanticManifest::path(data))?)];
    let reader = VectorIndex::open_read_only(&checkpoint)?;
    let query = HashEmbedder::default().embed_sync("compiler recovery evidence")?;
    assert_eq!(reader.wal_record_count(), 1, "fixture must retain the appended WAL record");
    let results = reader.search_top_k(&query, 200, None)?;
    assert_eq!(results.len(), 129);
    let hits = format!("{results:?}");
    drop(reader);
    let files = fallback_files(data, &checkpoint)?;
    let approval = plan_backfill_artifacts(data)?;
    assert_preserved(&before)?;
    apply_backfill_artifact_plan(data, &approval.plan_fingerprint)?;
    assert_preserved(&before)?;
    for (path, _) in files { assert!(!path.exists()); }
    let reopened = VectorIndex::open_read_only(&checkpoint)?;
    assert_eq!(hits, format!("{:?}", reopened.search_top_k(&query, 200, None)?));
    Ok(())
}

#[test]
fn missing_or_torn_acknowledged_wal_retains_fallbacks_instead_of_trusting_the_main_header() -> Result<()> {
    for missing in [true, false] {
        let temp = tempfile::tempdir()?;
        let data = temp.path();
        let (live, checkpoint, _) = setup_wal(data)?;
        let wal = wal_path_for(&checkpoint);
        let wal_bytes = fs::read(&wal)?;
        let files = fallback_files(data, &checkpoint)?;
        let before = [(checkpoint.clone(), fs::read(&checkpoint)?),
            (live.clone(), fs::read(&live)?),
            (SemanticManifest::path(data), fs::read(SemanticManifest::path(data))?)];
        let approval = plan_backfill_artifacts(data)?;
        if missing {
            fs::rename(&wal, data.join("retained-original.wal"))?;
        } else {
            fs::write(&wal, &wal_bytes[..wal_bytes.len() - 1])?;
        }
        let damaged_wal = fs::read(&wal).ok();
        assert!(plan_backfill_artifacts(data).is_err());
        let error = reclaim_backfill_artifacts(data).unwrap_err();
        assert!(format!("{error:#}").contains("retaining fallback artifacts"));
        assert!(apply_backfill_artifact_plan(data, &approval.plan_fingerprint).is_err());
        assert_preserved(&files)?;
        assert_preserved(&before)?;
        assert_eq!(fs::read(&wal).ok(), damaged_wal, "inspection must not repair the WAL");
        if missing { assert_eq!(fs::read(data.join("retained-original.wal"))?, wal_bytes); }
    }
    Ok(())
}

#[test]
fn appended_records_beyond_the_last_durable_checkpoint_remain_recoverable() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let (live, checkpoint, before_append) = setup_wal(data)?;
    // Simulate the crash cut after a WAL append but before checkpoint advance.
    fs::write(SemanticManifest::path(data), &before_append)?;
    assert_eq!(SemanticManifest::load(data)?.unwrap().checkpoint.unwrap().docs_embedded, 128);
    let wal = wal_path_for(&checkpoint);
    let before = [(checkpoint.clone(), fs::read(&checkpoint)?), (wal.clone(), fs::read(&wal)?),
        (live.clone(), fs::read(&live)?), (SemanticManifest::path(data), before_append)];
    let files = fallback_files(data, &checkpoint)?;
    let approval = plan_backfill_artifacts(data)?;
    apply_backfill_artifact_plan(data, &approval.plan_fingerprint)?;
    assert_preserved(&before)?;
    for (path, _) in files { assert!(!path.exists()); }
    let reader = VectorIndex::open_read_only(&checkpoint)?;
    assert_eq!(reader.wal_record_count(), 1);
    let query = HashEmbedder::default().embed_sync("compiler recovery evidence")?;
    assert_eq!(reader.search_top_k(&query, 200, None)?.len(), 129);
    Ok(())
}
