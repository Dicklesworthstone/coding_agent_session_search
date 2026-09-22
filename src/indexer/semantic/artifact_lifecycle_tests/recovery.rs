//! Crash recovery and alias/sidecar ownership regressions for #490.

use super::*;
use anyhow::{Context, ensure};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::search::semantic_manifest::{SemanticCurrentPointerV1, SemanticShardManifest};

const CRASH_ROOT_ENV: &str = "CASS_TEST_BACKFILL_ARTIFACT_CRASH_ROOT";

/// This helper is selected explicitly in a child process. Normal test runs do
/// nothing here; the parent kills the child so Rust destructors cannot clean up.
#[test]
fn interrupted_backfill_child() -> Result<()> {
    let Ok(data) = dotenvy::var(CRASH_ROOT_ENV) else {
        return Ok(());
    };
    let data = PathBuf::from(data);
    let mut manifest = SemanticManifest::load(&data)?.context("child manifest missing")?;
    let indexer = SemanticIndexer::new("hash", None)?;
    indexer.with_backfill_artifacts(&data, &mut manifest, |_, _| {
        orphan(&data.join(VECTOR_INDEX_DIR))?;
        fs::write(data.join("child-ready"), b"artifacts owned")?;
        loop {
            std::thread::park();
        }
    })?;
    Ok(())
}

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn killed_process_releases_lease_and_next_backfill_recovers_its_scratch() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let root = data.join(VECTOR_INDEX_DIR);
    let indexer = SemanticIndexer::new("hash", None)?;
    let mut manifest = SemanticManifest::default();
    let live = indexer.run_backfill_batch(
        &rows(3),
        data,
        &mut manifest,
        plan("content-v1:live", 3, true),
    )?;
    let live_bytes = fs::read(&live.index_path)?;
    let current = checkpoint(&indexer, data, &mut manifest)?;
    let current_bytes = fs::read(&current)?;
    let helper = concat!(module_path!(), "::interrupted_backfill_child");
    let (_, filter) = helper
        .split_once("::")
        .context("test module has no crate")?;
    let mut child = KillOnDrop(
        Command::new(std::env::current_exe()?)
            .arg("--exact")
            .arg(filter)
            .env(CRASH_ROOT_ENV, data)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?,
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    while !data.join("child-ready").is_file() {
        ensure!(
            child.0.try_wait()?.is_none(),
            "child exited before acquiring its lease"
        );
        ensure!(
            Instant::now() < deadline,
            "child did not signal artifact ownership"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(reclaim_backfill_artifacts(data).is_err());
    assert_eq!(fs::read(&current)?, current_bytes);
    assert_eq!(fs::read(&live.index_path)?, live_bytes);
    child.0.kill()?;
    assert!(!child.0.wait()?.success());
    let staging = root.join(".staging-quality-minilm-384-deadbeef.fsvi");
    let reuse = root.join(".backfill-reuse-Killed123");
    assert!(
        staging.is_file() && reuse.is_dir(),
        "kill must bypass cleanup"
    );

    // Exercise automatic startup recovery, not only the explicit maintenance
    // API. A subsequent injected error prevents the after-success cleanup path.
    let error = indexer.with_backfill_artifacts(data, &mut manifest, |_, _| {
        assert!(!staging.exists() && !reuse.exists());
        assert_eq!(fs::read(&current)?, current_bytes);
        assert_eq!(fs::read(&live.index_path)?, live_bytes);
        bail!("stop after startup recovery")
    });
    assert!(error.is_err());
    assert_eq!(VectorIndex::open_read_only(&current)?.record_count(), 1);
    assert_eq!(
        VectorIndex::open_read_only(&live.index_path)?.record_count(),
        3
    );
    Ok(())
}

#[test]
fn shard_references_protect_staging_names_and_containing_reuse_directories() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let root = data.join(VECTOR_INDEX_DIR);
    let indexer = SemanticIndexer::new("hash", None)?;
    let built = indexer.build_and_save_index_shards(
        indexer.embed_messages(&rows(2))?,
        data,
        SemanticShardBuildPlan {
            tier: TierKind::Quality,
            db_fingerprint: "content-v1:shards".into(),
            model_revision: "hash".into(),
            total_conversations: 2,
            max_records_per_shard: 1,
            build_ann: true,
        },
    )?;
    assert!(built.complete);
    let named_live = root.join(".staging-quality-fnv1a-384-12345678.fsvi");
    fs::copy(&built.index_paths[0], &named_live)?;
    let reuse_live = root.join(".backfill-reuse-Published123");
    fs::create_dir_all(&reuse_live)?;
    let ann = reuse_live.join("live.chsw");
    fs::copy(&built.ann_index_paths[0], &ann)?;
    let before = fs::read(&named_live)?;
    let ann_before = fs::read(&ann)?;
    let mut shards = SemanticShardManifest::load(data)?.unwrap();
    shards.shards[0].index_path = named_live
        .strip_prefix(data)?
        .to_string_lossy()
        .into_owned();
    shards.shards[0].ann_index_path = Some(ann.strip_prefix(data)?.to_string_lossy().into_owned());
    shards.shards[0].ready = false;
    shards.shards[0].ann_ready = false;
    shards.save(data)?;
    let (stale, reuse_stale) = orphan(&root)?;
    reclaim_backfill_artifacts(data)?;
    assert!(!stale.exists() && !reuse_stale.exists());
    assert_eq!(fs::read(&named_live)?, before);
    assert_eq!(fs::read(&ann)?, ann_before);
    assert_eq!(VectorIndex::open_read_only(&named_live)?.record_count(), 1);
    for path in built.index_paths.iter().chain(&built.ann_index_paths) {
        assert!(path.is_file());
    }
    Ok(())
}

#[test]
fn wal_only_orphans_are_recovered_without_requiring_a_surviving_main_file() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().join(VECTOR_INDEX_DIR);
    fs::create_dir_all(&root)?;
    let wal = wal_path_for(&stage(&root, "content-v1:orphan-wal"));
    fs::write(&wal, b"orphaned WAL")?;
    let report = reclaim_backfill_artifacts(temp.path())?;
    assert_eq!(report.removed_files, 1);
    assert_eq!(report.reclaimed_bytes, 12);
    assert!(!wal.exists());
    Ok(())
}

#[test]
fn invalid_selected_generation_never_authorizes_deletion() -> Result<()> {
    for (version, generation) in [(999, "future"), (1, "../escape"), (1, "missing")] {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join(VECTOR_INDEX_DIR);
        fs::create_dir_all(&root)?;
        let (staging, reuse) = orphan(&root)?;
        let pointer = SemanticCurrentPointerV1 {
            schema_version: version,
            generation_id: generation.into(),
            manifest_sha256: "0".repeat(64),
            selection_epoch: 1,
            selected_at_ms: 1,
        };
        fs::write(root.join("current.json"), serde_json::to_vec(&pointer)?)?;
        assert!(reclaim_backfill_artifacts(temp.path()).is_err());
        assert!(staging.exists() && reuse.exists());
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn recovery_does_not_follow_scratch_symlinks_or_delete_manifest_alias_targets() -> Result<()> {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;
    let data = temp.path();
    let root = data.join(VECTOR_INDEX_DIR);
    let indexer = SemanticIndexer::new("hash", None)?;
    let mut manifest = SemanticManifest::default();
    let live = indexer.run_backfill_batch(
        &rows(3),
        data,
        &mut manifest,
        plan("content-v1:live", 3, true),
    )?;
    let target = root.join(".staging-quality-fnv1a-384-12345678.fsvi");
    fs::copy(&live.index_path, &target)?;
    let before = fs::read(&target)?;
    let alias = root.join("published-alias.fsvi");
    symlink(&target, &alias)?;
    manifest.quality_tier.as_mut().unwrap().index_path =
        alias.strip_prefix(data)?.to_string_lossy().into_owned();
    manifest.save(data)?;
    fs::write(outside.path().join("live"), b"external live data")?;
    let link = root.join(".backfill-reuse-External123");
    symlink(outside.path(), &link)?;
    let file_link = root.join(".staging-quality-minilm-384-00000000.fsvi");
    symlink(outside.path().join("live"), &file_link)?;
    fs::write(wal_path_for(&file_link), b"linked sidecar")?;
    let dangling = root.join(".staging-fast-fnv1a-384-00000001.fsvi");
    symlink(outside.path().join("absent"), &dangling)?;
    let (staging, reuse) = orphan(&root)?;
    symlink(outside.path(), reuse.join("external-child"))?;
    reclaim_backfill_artifacts(data)?;
    assert!(!staging.exists() && !reuse.exists());
    assert_eq!(fs::read(&target)?, before);
    assert_eq!(VectorIndex::open_read_only(&alias)?.record_count(), 3);
    assert_eq!(
        fs::read(outside.path().join("live"))?,
        b"external live data"
    );
    assert!(link.is_symlink() && file_link.is_symlink() && dangling.is_symlink());
    assert_eq!(fs::read(wal_path_for(&file_link))?, b"linked sidecar");
    Ok(())
}

#[cfg(unix)]
#[test]
fn broken_manifest_alias_retains_every_candidate_for_manual_inspection() -> Result<()> {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let root = data.join(VECTOR_INDEX_DIR);
    let indexer = SemanticIndexer::new("hash", None)?;
    let mut manifest = SemanticManifest::default();
    indexer.run_backfill_batch(
        &rows(3),
        data,
        &mut manifest,
        plan("content-v1:live", 3, true),
    )?;
    let alias = root.join("published-alias.fsvi");
    symlink(root.join("absent.fsvi"), &alias)?;
    manifest.quality_tier.as_mut().unwrap().index_path =
        alias.strip_prefix(data)?.to_string_lossy().into_owned();
    manifest.save(data)?;
    let (staging, reuse) = orphan(&root)?;
    assert!(reclaim_backfill_artifacts(data).is_err());
    assert!(staging.exists() && reuse.exists() && alias.is_symlink());
    Ok(())
}
