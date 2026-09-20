//! Real file-backed regressions for #490. Hash vectors exercise lifecycle and
//! search preservation without requiring a downloaded quality-tier model.

use super::*;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use frankensearch::index::{VectorIndex, wal_path_for};

use crate::search::embedder::Embedder;
use crate::search::hash_embedder::HashEmbedder;
use crate::search::semantic_manifest::{HnswRecord, TierKind};
use crate::search::vector_index::VECTOR_INDEX_DIR;

fn plan(fingerprint: &str, rows: u64, complete: bool) -> SemanticBackfillBatchPlan {
    SemanticBackfillBatchPlan {
        tier: TierKind::Quality,
        db_fingerprint: fingerprint.into(),
        model_revision: "hash".into(),
        total_conversations: 3,
        conversations_in_batch: rows,
        last_offset: if complete { 3 } else { rows as i64 },
        cursor_exhausted: complete,
    }
}

fn rows(count: u64) -> Vec<EmbeddingInput> {
    (1..=count)
        .map(|id| EmbeddingInput::new(id, format!("durable compiler checkpoint {id}")))
        .collect()
}

fn checkpoint(
    indexer: &SemanticIndexer,
    data: &Path,
    manifest: &mut SemanticManifest,
) -> Result<PathBuf> {
    Ok(indexer
        .run_backfill_batch(&rows(1), data, manifest, plan("content-v1:first", 1, false))?
        .index_path)
}

fn stage(root: &Path, fingerprint: &str) -> PathBuf {
    root.join(format!(
        ".staging-quality-fnv1a-384-{:08x}.fsvi",
        crc32fast::hash(fingerprint.as_bytes())
    ))
}

fn orphan(root: &Path) -> Result<(PathBuf, PathBuf)> {
    let staging = root.join(".staging-quality-minilm-384-deadbeef.fsvi");
    fs::write(&staging, b"old staging")?;
    let reuse = root.join(".backfill-reuse-Killed123");
    fs::create_dir_all(&reuse)?;
    fs::write(reuse.join("candidate.fsvi"), b"old reuse")?;
    Ok((staging, reuse))
}

fn hold_lock(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    fs2::FileExt::try_lock_exclusive(&file)?;
    Ok(file)
}

#[test]
fn superseded_checkpoints_are_reclaimed_only_after_replacement_and_publication() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let indexer = SemanticIndexer::new("hash", None)?;
    let mut manifest = SemanticManifest::default();
    let first = checkpoint(&indexer, data, &mut manifest)?;
    assert!(first.is_file());
    let first_wal = wal_path_for(&first);
    // Reclamation treats a staging main/WAL as one ownership unit. This WAL
    // belongs to the previous fingerprint, never the replacement's input.
    fs::write(&first_wal, b"superseded WAL")?;
    let second = indexer.run_backfill_batch(
        &rows(2),
        data,
        &mut manifest,
        plan("content-v1:second", 2, false),
    )?;
    assert!(second.checkpoint_saved);
    assert_ne!(first, second.index_path);
    assert!(!first.exists());
    assert!(!first_wal.exists());
    assert_eq!(
        VectorIndex::open_read_only(&second.index_path)?.record_count(),
        2
    );
    let saved = SemanticManifest::load(data)?.unwrap();
    assert_eq!(saved.checkpoint.unwrap().db_fingerprint, "content-v1:second");

    let published = indexer.run_backfill_batch(
        &rows(3)[2..],
        data,
        &mut manifest,
        plan("content-v1:second", 1, true),
    )?;
    assert!(published.published);
    assert!(!second.index_path.exists());
    assert_eq!(
        VectorIndex::open_read_only(&published.index_path)?.record_count(),
        3
    );
    assert!(SemanticManifest::load(data)?.unwrap().checkpoint.is_none());
    assert!(reclaim_backfill_artifacts(data)?.failed_paths.is_empty());
    assert!(published.index_path.is_file());
    Ok(())
}

#[test]
fn reclaim_preserves_live_manifest_artifacts_wals_and_open_search_readers() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let root = data.join(VECTOR_INDEX_DIR);
    let indexer = SemanticIndexer::new("hash", None)?;
    let mut manifest = SemanticManifest::default();
    let published = indexer.run_backfill_batch(
        &rows(3),
        data,
        &mut manifest,
        plan("content-v1:live", 3, true),
    )?;
    let reader = VectorIndex::open_read_only(&published.index_path)?;
    let query = HashEmbedder::default().embed_sync("durable compiler checkpoint")?;
    let before_search = reader.search_top_k(&query, 3, None)?;
    let live_bytes = fs::read(&published.index_path)?;

    // A staging-looking name is not proof of garbage. Readiness does not
    // change ownership either: keep a stale fast record and an ANN reference.
    let named_live = root.join(".staging-fast-fnv1a-384-1234abcd.fsvi");
    fs::copy(&published.index_path, &named_live)?;
    fs::write(wal_path_for(&named_live), b"live sidecar")?;
    let mut fast = manifest.quality_tier.clone().unwrap();
    fast.tier = TierKind::Fast;
    fast.ready = false;
    fast.index_path = "vector_index/./.staging-fast-fnv1a-384-1234abcd.fsvi".into();
    manifest.fast_tier = Some(fast);
    let ann = root.join(".staging-quality-minilm-384-abcdef12.fsvi");
    fs::write(&ann, b"live ANN artifact")?;
    manifest.hnsw = Some(HnswRecord {
        base_tier: TierKind::Quality,
        embedder_id: "fnv1a-384".into(),
        ef_search: 16,
        index_path: ann.strip_prefix(data)?.to_string_lossy().into_owned(),
        size_bytes: 17,
        built_at_ms: 1,
        ready: false,
    });
    manifest.save(data)?;
    let manifest_bytes = fs::read(SemanticManifest::path(data))?;
    let (staging, reuse) = orphan(&root)?;
    let orphan_wal = wal_path_for(&staging);
    fs::write(&orphan_wal, b"old WAL")?;
    let mut untouched = Vec::new();
    for name in [
        "index-unreferenced.fsvi",
        ".staging-quality-minilm-384-not-a-hash.fsvi",
        ".backfill-reuse-not-a-directory",
        "generations/live/index.fsvi",
        "shards/live/index.fsvi",
        ".quarantined-destination-wal-evidence/destination.wal",
    ] {
        let path = root.join(name);
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(&path, b"must survive")?;
        untouched.push(path);
    }
    let report = reclaim_backfill_artifacts(data)?;
    assert_eq!(report.removed_files, 2);
    assert_eq!(report.removed_directories, 1);
    assert_eq!(report.reclaimed_bytes, 27);
    assert!(report.failed_paths.is_empty());
    assert!(!staging.exists() && !orphan_wal.exists() && !reuse.exists());
    assert_eq!(fs::read(&published.index_path)?, live_bytes);
    assert_eq!(fs::read(&named_live)?, live_bytes);
    assert_eq!(fs::read(wal_path_for(&named_live))?, b"live sidecar");
    assert_eq!(fs::read(&ann)?, b"live ANN artifact");
    assert_eq!(fs::read(SemanticManifest::path(data))?, manifest_bytes);
    for path in untouched {
        assert_eq!(fs::read(path)?, b"must survive");
    }
    assert_eq!(reader.search_top_k(&query, 3, None)?, before_search);
    assert_eq!(
        VectorIndex::open_read_only(&published.index_path)?.search_top_k(&query, 3, None)?,
        before_search,
    );
    let again = reclaim_backfill_artifacts(data)?;
    assert_eq!(again.removed_files + again.removed_directories, 0);
    Ok(())
}

#[test]
fn failed_checkpoint_save_keeps_old_and_new_staging_until_recovery() -> Result<()> {
    for renamed in [false, true] {
        let temp = tempfile::tempdir()?;
        let data = temp.path();
        let indexer = SemanticIndexer::new("hash", None)?;
        let mut manifest = SemanticManifest::default();
        let old = checkpoint(&indexer, data, &mut manifest)?;
        let new = stage(&data.join(VECTOR_INDEX_DIR), "content-v1:replacement");
        let bytes = fs::read(&old)?;
        let failure = indexer.with_backfill_artifacts(data, &mut manifest, |_, manifest| {
            fs::copy(&old, &new)?;
            manifest.checkpoint.as_mut().unwrap().db_fingerprint =
                "content-v1:replacement".into();
            if renamed {
                // Simulate visibility of the new manifest before its directory
                // fsync succeeds. An error must NOT turn this into a GC commit.
                fs::write(SemanticManifest::path(data), serde_json::to_vec(manifest)?)?;
            }
            bail!("injected checkpoint durability failure")
        });
        assert!(failure.is_err());
        assert_eq!(fs::read(&old)?, bytes);
        assert_eq!(fs::read(&new)?, bytes);
        let report = reclaim_backfill_artifacts(data)?;
        assert_eq!(report.removed_files, 1);
        let (kept, removed) = if renamed { (&new, &old) } else { (&old, &new) };
        assert_eq!(fs::read(kept)?, bytes);
        assert!(!removed.exists());
        assert_eq!(VectorIndex::open_read_only(kept)?.record_count(), 1);
    }
    Ok(())
}

#[test]
fn reclamation_obeys_both_locks_and_does_not_unlink_lock_files() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let root = data.join(VECTOR_INDEX_DIR);
    fs::create_dir_all(&root)?;
    let (staging, reuse) = orphan(&root)?;
    let indexer = SemanticIndexer::new("hash", None)?;
    for name in ["index-run.lock", "semantic-backfill-artifacts.lock"] {
        let path = data.join(name);
        fs::write(&path, b"prior owner metadata")?;
        let owner = hold_lock(&path)?;
        assert!(reclaim_backfill_artifacts(data).is_err());
        if name == "semantic-backfill-artifacts.lock" {
            assert!(
                indexer
                    .run_backfill_batch(
                        &rows(1),
                        data,
                        &mut SemanticManifest::default(),
                        plan("new", 1, false),
                    )
                    .is_err()
            );
        }
        assert!(staging.is_file() && reuse.is_dir());
        assert_eq!(fs::read(&path)?, b"prior owner metadata");
        drop(owner);
        assert!(path.is_file());
    }
    let report = reclaim_backfill_artifacts(data)?;
    assert_eq!(report.removed_files, 1);
    assert_eq!(report.removed_directories, 1);
    Ok(())
}

#[test]
fn missing_checkpoint_retains_possible_fallbacks_instead_of_guessing() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let indexer = SemanticIndexer::new("hash", None)?;
    let mut manifest = SemanticManifest::default();
    let old = checkpoint(&indexer, data, &mut manifest)?;
    manifest.checkpoint.as_mut().unwrap().db_fingerprint = "missing".into();
    manifest.save(data)?;
    let (staging, reuse) = orphan(&data.join(VECTOR_INDEX_DIR))?;
    let report = reclaim_backfill_artifacts(data)?;
    assert!(report.checkpoint_missing);
    assert_eq!(report.removed_files + report.removed_directories, 0);
    assert!(old.exists() && staging.exists() && reuse.exists());
    Ok(())
}

#[test]
fn malformed_and_future_metadata_fail_closed_before_any_deletion() -> Result<()> {
    for (name, bytes) in [
        ("semantic_manifest.json", b"broken JSON".to_vec()),
        ("semantic_shards.json", b"broken JSON".to_vec()),
        ("current.json", b"broken JSON".to_vec()),
        ("semantic_manifest.json", {
            let mut future = SemanticManifest::default();
            future.manifest_version = u32::MAX;
            serde_json::to_vec(&future)?
        }),
        ("semantic_shards.json", serde_json::to_vec(&serde_json::json!({
            "manifest_version": 999, "shards": [], "updated_at_ms": 0
        }))?),
    ] {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join(VECTOR_INDEX_DIR);
        fs::create_dir_all(&root)?;
        let (staging, reuse) = orphan(&root)?;
        fs::write(root.join(name), bytes)?;
        assert!(reclaim_backfill_artifacts(temp.path()).is_err(), "{name}");
        assert!(staging.exists() && reuse.exists(), "{name}");
    }
    Ok(())
}

#[test]
fn lifecycle_boundary_preserves_associated_method_calls() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?.with_batch_size(2)?;
    assert_eq!(SemanticIndexer::batch_size(&indexer), 2);
    assert_eq!(SemanticIndexer::embedder_id(&indexer), "fnv1a-384");
    assert_eq!(SemanticIndexer::embedder_dimension(&indexer), 384);
    let embedded = SemanticIndexer::embed_messages(&indexer, &rows(1))?;
    let index = SemanticIndexer::build_and_save_index(&indexer, embedded, temp.path())?;
    assert_eq!(index.record_count(), 1);
    Ok(())
}

mod recovery;
