//! File-backed approval/ownership regressions; no model downloads or canonical DB.
use super::*;
use crate::indexer::semantic::{EmbeddingInput, SemanticBackfillBatchPlan, SemanticIndexer};
use crate::search::embedder::Embedder;
use crate::search::hash_embedder::HashEmbedder;
use crate::search::semantic_manifest::TierKind;
use frankensearch::index::VectorIndex;

fn publish(data: &Path) -> Result<PathBuf> {
    let indexer = SemanticIndexer::new("hash", None)?;
    let mut manifest = SemanticManifest::default();
    Ok(indexer
        .run_backfill_batch(
            &[EmbeddingInput::new(
                1,
                "compiler checkpoint recovery evidence",
            )],
            data,
            &mut manifest,
            SemanticBackfillBatchPlan {
                tier: TierKind::Quality,
                db_fingerprint: "content-v1:approval-live".into(),
                model_revision: "hash".into(),
                total_conversations: 1,
                conversations_in_batch: 1,
                last_offset: 1,
                cursor_exhausted: true,
            },
        )?
        .index_path)
}

fn scratch(data: &Path) -> Result<(PathBuf, PathBuf)> {
    let root = data.join(VECTOR_INDEX_DIR);
    let staging = root.join(".staging-quality-minilm-384-deadbeef.fsvi");
    fs::write(&staging, b"obsolete staging")?;
    fs::write(wal_path_for(&staging), b"obsolete WAL")?;
    let reuse = root.join(".backfill-reuse-Killed123");
    fs::create_dir(&reuse)?;
    fs::write(reuse.join("candidate.fsvi"), b"private copy")?;
    Ok((staging, reuse))
}

#[test]
fn preview_and_apply_agree_and_preserve_live_bytes_and_fresh_search() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let live = publish(data)?;
    let (staging, reuse) = scratch(data)?;
    let main_before = fs::read(&live)?;
    let metadata_before = fs::read(SemanticManifest::path(data))?;
    let reader = VectorIndex::open_read_only(&live)?;
    let query = HashEmbedder::default().embed_sync("compiler checkpoint recovery evidence")?;
    let hits_before = format!("{:?}", reader.search_top_k(&query, 3, None)?);
    let plan = plan_backfill_artifacts(data)?;
    assert_eq!(
        (plan.reclaimable_files, plan.reclaimable_directories),
        (2, 1)
    );
    assert_eq!(plan.reclaimable_bytes, 16 + 12 + 12);
    assert_eq!(plan.candidates.len(), 3);
    assert_eq!(
        plan.plan_fingerprint,
        plan_backfill_artifacts(data)?.plan_fingerprint
    );
    assert_eq!(main_before, fs::read(&live)?);
    assert_eq!(metadata_before, fs::read(SemanticManifest::path(data))?);
    assert_eq!(fs::read(&staging)?, b"obsolete staging");
    assert_eq!(fs::read(reuse.join("candidate.fsvi"))?, b"private copy");
    let report = apply_backfill_artifact_plan(data, &plan.plan_fingerprint)?;
    assert_eq!((report.removed_files, report.removed_directories), (2, 1));
    assert_eq!(report.reclaimed_bytes, plan.reclaimable_bytes);
    assert!(report.failed_paths.is_empty());
    assert!(!staging.exists() && !wal_path_for(&staging).exists() && !reuse.exists());
    assert_eq!(main_before, fs::read(&live)?);
    assert_eq!(metadata_before, fs::read(SemanticManifest::path(data))?);
    assert_eq!(
        hits_before,
        format!("{:?}", reader.search_top_k(&query, 3, None)?)
    );
    let reopened = VectorIndex::open_read_only(&live)?;
    assert_eq!(
        hits_before,
        format!("{:?}", reopened.search_top_k(&query, 3, None)?)
    );
    assert!(apply_backfill_artifact_plan(data, &plan.plan_fingerprint).is_err());
    assert!(plan_backfill_artifacts(data)?.candidates.is_empty());
    Ok(())
}

#[test]
fn newly_referenced_staging_invalidates_approval_before_any_removal() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    publish(data)?;
    let (staging, reuse) = scratch(data)?;
    let plan = plan_backfill_artifacts(data)?;
    let mut manifest = SemanticManifest::load(data)?.unwrap();
    let artifact = manifest.quality_tier.as_mut().unwrap();
    artifact.index_path = staging.strip_prefix(data)?.to_string_lossy().into_owned();
    artifact.ready = false; // Not-ready still owns its main/WAL pair.
    manifest.save(data)?;
    let metadata_before = fs::read(SemanticManifest::path(data))?;
    assert!(apply_backfill_artifact_plan(data, &plan.plan_fingerprint).is_err());
    assert!(staging.is_file() && wal_path_for(&staging).is_file() && reuse.is_dir());
    assert_eq!(metadata_before, fs::read(SemanticManifest::path(data))?);
    let refreshed = plan_backfill_artifacts(data)?;
    assert_eq!(
        (
            refreshed.reclaimable_files,
            refreshed.reclaimable_directories
        ),
        (0, 1)
    );
    apply_backfill_artifact_plan(data, &refreshed.plan_fingerprint)?;
    assert!(staging.is_file() && wal_path_for(&staging).is_file());
    Ok(())
}

#[test]
fn any_authority_change_invalidates_approval_even_when_candidates_match() -> Result<()> {
    let temp = tempfile::tempdir()?;
    publish(temp.path())?;
    let (staging, reuse) = scratch(temp.path())?;
    let plan = plan_backfill_artifacts(temp.path())?;
    let mut manifest = SemanticManifest::load(temp.path())?.unwrap();
    manifest.backlog.total_conversations += 1;
    manifest.save(temp.path())?;
    let refreshed = plan_backfill_artifacts(temp.path())?;
    assert_eq!(plan.candidates, refreshed.candidates);
    assert_ne!(plan.plan_fingerprint, refreshed.plan_fingerprint);
    assert!(apply_backfill_artifact_plan(temp.path(), &plan.plan_fingerprint).is_err());
    assert!(staging.is_file() && reuse.is_dir());
    Ok(())
}

#[test]
fn new_or_changed_scratch_invalidates_the_entire_plan() -> Result<()> {
    for nested in [false, true] {
        let temp = tempfile::tempdir()?;
        let data = temp.path();
        publish(data)?;
        let (staging, reuse) = scratch(data)?;
        let plan = plan_backfill_artifacts(data)?;
        if nested {
            fs::write(reuse.join("candidate.fsvi"), b"new private data")?;
        } else {
            fs::write(
                data.join(VECTOR_INDEX_DIR)
                    .join(".staging-fast-fnv1a-384-12345678.fsvi"),
                b"new",
            )?;
        }
        assert!(apply_backfill_artifact_plan(data, &plan.plan_fingerprint).is_err());
        assert!(staging.is_file() && wal_path_for(&staging).is_file() && reuse.is_dir());
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn restored_mtime_does_not_hide_a_same_length_scratch_rewrite() -> Result<()> {
    let temp = tempfile::tempdir()?;
    publish(temp.path())?;
    let (staging, reuse) = scratch(temp.path())?;
    let modified = fs::metadata(&staging)?.modified()?;
    let plan = plan_backfill_artifacts(temp.path())?;
    fs::write(&staging, b"changed! staging")?;
    File::options()
        .write(true)
        .open(&staging)?
        .set_modified(modified)?;
    assert!(apply_backfill_artifact_plan(temp.path(), &plan.plan_fingerprint).is_err());
    assert!(staging.is_file() && reuse.is_dir());
    Ok(())
}

#[test]
fn approval_is_bound_to_the_archive_and_both_writer_locks() -> Result<()> {
    let first = tempfile::tempdir()?;
    let second = tempfile::tempdir()?;
    publish(first.path())?;
    publish(second.path())?;
    scratch(first.path())?;
    let (staging, reuse) = scratch(second.path())?;
    let plan = plan_backfill_artifacts(first.path())?;
    assert!(apply_backfill_artifact_plan(second.path(), &plan.plan_fingerprint).is_err());
    let plan = plan_backfill_artifacts(second.path())?;
    for name in ["index-run.lock", ARTIFACT_LOCK] {
        let owner = lock_file(&second.path().join(name))?;
        assert!(plan_backfill_artifacts(second.path()).is_err());
        assert!(apply_backfill_artifact_plan(second.path(), &plan.plan_fingerprint).is_err());
        assert!(staging.is_file() && reuse.is_dir());
        drop(owner);
    }
    Ok(())
}

#[test]
fn malformed_or_future_metadata_never_becomes_an_empty_ownership_set() -> Result<()> {
    for kind in 0..4 {
        let temp = tempfile::tempdir()?;
        let data = temp.path();
        publish(data)?;
        let (staging, reuse) = scratch(data)?;
        let plan = plan_backfill_artifacts(data)?;
        let path = match kind {
            0 | 3 => SemanticManifest::path(data),
            1 => SemanticShardManifest::path(data),
            _ => SemanticCurrentPointerV1::path(data),
        };
        if kind == 3 {
            let mut manifest = SemanticManifest::load(data)?.unwrap();
            manifest.manifest_version = u32::MAX;
            manifest.save(data)?;
        } else {
            fs::write(&path, b"{truncated")?;
        }
        let before = fs::read(&path)?;
        assert!(plan_backfill_artifacts(data).is_err());
        assert!(apply_backfill_artifact_plan(data, &plan.plan_fingerprint).is_err());
        assert_eq!(before, fs::read(path)?);
        assert!(staging.is_file() && reuse.is_dir());
    }
    Ok(())
}

#[test]
fn missing_checkpoint_reports_blocked_and_keeps_fallback_artifacts() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let indexer = SemanticIndexer::new("hash", None)?;
    let mut manifest = SemanticManifest::default();
    let outcome = indexer.run_backfill_batch(
        &[EmbeddingInput::new(1, "resumable checkpoint")],
        data,
        &mut manifest,
        SemanticBackfillBatchPlan {
            tier: TierKind::Quality,
            db_fingerprint: "content-v1:checkpoint".into(),
            model_revision: "hash".into(),
            total_conversations: 2,
            conversations_in_batch: 1,
            last_offset: 1,
            cursor_exhausted: false,
        },
    )?;
    fs::rename(
        outcome.index_path,
        data.join(VECTOR_INDEX_DIR).join("fallback.fsvi"),
    )?;
    let (staging, reuse) = scratch(data)?;
    let plan = plan_backfill_artifacts(data)?;
    assert!(plan.checkpoint_missing && plan.candidates.is_empty());
    assert!(apply_backfill_artifact_plan(data, &plan.plan_fingerprint).is_err());
    assert!(staging.is_file() && reuse.is_dir());
    Ok(())
}

#[test]
fn absent_archive_or_vector_root_is_not_created_by_preview() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let absent = temp.path().join("not-an-archive");
    assert!(plan_backfill_artifacts(&absent).is_err());
    assert!(!absent.exists());
    assert!(plan_backfill_artifacts(temp.path()).is_err());
    assert!(!temp.path().join(VECTOR_INDEX_DIR).exists());
    assert!(!temp.path().join("index-run.lock").exists());
    Ok(())
}

#[test]
fn changed_candidate_type_cannot_escalate_unlink_to_recursive_deletion() -> Result<()> {
    let temp = tempfile::tempdir()?;
    publish(temp.path())?;
    let root = temp.path().join(VECTOR_INDEX_DIR);
    let staging = root.join(".staging-quality-minilm-384-deadbeef.fsvi");
    fs::write(&staging, b"old")?;
    let artifacts = BackfillArtifacts::lock(temp.path())?;
    let discovery = artifacts.discover(None, None, false)?;
    fs::rename(&staging, root.join("retained-original.fsvi"))?;
    fs::create_dir(&staging)?;
    fs::write(staging.join("keep"), b"not scratch")?;
    let report = artifacts.remove_candidates(discovery.candidates)?;
    assert_eq!(report.failed_paths, vec![staging.clone()]);
    assert_eq!(report.removed_directories, 0);
    assert_eq!(fs::read(staging.join("keep"))?, b"not scratch");
    Ok(())
}

#[cfg(unix)]
#[test]
fn nested_links_are_not_followed_and_referenced_aliases_are_not_reclaimed() -> Result<()> {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;
    fs::write(outside.path().join("evidence"), b"external")?;
    let data = temp.path();
    publish(data)?;
    let (staging, reuse) = scratch(data)?;
    symlink(outside.path(), reuse.join("external"))?;
    let alias = data.join(VECTOR_INDEX_DIR).join("published-alias.fsvi");
    symlink(&staging, &alias)?;
    let mut manifest = SemanticManifest::load(data)?.unwrap();
    manifest.quality_tier.as_mut().unwrap().index_path =
        alias.strip_prefix(data)?.to_string_lossy().into_owned();
    manifest.save(data)?;
    let plan = plan_backfill_artifacts(data)?;
    assert_eq!(
        (plan.reclaimable_files, plan.reclaimable_directories),
        (0, 1)
    );
    assert_eq!(plan.reclaimable_bytes, 12);
    apply_backfill_artifact_plan(data, &plan.plan_fingerprint)?;
    assert!(staging.is_file() && wal_path_for(&staging).is_file());
    assert_eq!(fs::read(outside.path().join("evidence"))?, b"external");
    Ok(())
}

#[test]
fn inventory_budget_and_nonregular_checkpoint_wal_fail_closed() -> Result<()> {
    let temp = tempfile::tempdir()?;
    publish(temp.path())?;
    let (staging, reuse) = scratch(temp.path())?;
    assert!(snapshot(temp.path(), &reuse, &mut 1).is_err());
    let main = temp.path().join("checkpoint.fsvi");
    fs::write(&main, b"checkpoint")?;
    fs::create_dir(wal_path_for(&main))?;
    assert!(inspect_checkpoint(&main, false).is_err());
    assert!(staging.is_file() && reuse.is_dir());
    Ok(())
}

#[cfg(unix)]
#[test]
fn automatic_recovery_still_supports_non_utf8_data_directories() -> Result<()> {
    use std::os::unix::ffi::OsStringExt;
    let temp = tempfile::tempdir()?;
    let data = temp
        .path()
        .join(std::ffi::OsString::from_vec(b"archive-\xff".to_vec()));
    let live = publish(&data)?;
    let (staging, reuse) = scratch(&data)?;
    let report = reclaim_backfill_artifacts(&data)?;
    assert_eq!((report.removed_files, report.removed_directories), (2, 1));
    assert!(!staging.exists() && !reuse.exists());
    assert!(live.is_file());
    Ok(())
}
