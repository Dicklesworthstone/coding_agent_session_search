//! A serialized writer must not publish or reclaim from a pre-lease manifest.
//! All fixtures use real FSVI files and the hash producer, without model assets.

use super::*;
use std::collections::BTreeMap;

use crate::indexer::semantic::{EmbeddingInput, SemanticBackfillBatchPlan, SemanticIndexer};
use crate::search::embedder::Embedder;
use crate::search::hash_embedder::HashEmbedder;
use crate::search::semantic_manifest::{HnswRecord, TierKind};
use frankensearch::index::VectorIndex;

fn plan(fingerprint: &str, offset: i64, count: u64, complete: bool) -> SemanticBackfillBatchPlan {
    SemanticBackfillBatchPlan {
        tier: TierKind::Quality,
        db_fingerprint: fingerprint.into(),
        model_revision: "hash".into(),
        total_conversations: 3,
        conversations_in_batch: count,
        last_offset: offset,
        cursor_exhausted: complete,
    }
}

fn rows(ids: std::ops::Range<u64>) -> Vec<EmbeddingInput> {
    ids.map(|id| EmbeddingInput::new(id, format!("compiler recovery lease document {id}")))
        .collect()
}

fn publish(data: &Path, manifest: &mut SemanticManifest) -> Result<PathBuf> {
    Ok(SemanticIndexer::new("hash", None)?
        .run_backfill_batch(&rows(1..4), data, manifest, plan("live", 3, 3, true))?
        .index_path)
}

fn scratch(data: &Path) -> Result<()> {
    let root = data.join(VECTOR_INDEX_DIR);
    fs::write(
        root.join(".staging-quality-fnv1a-384-deadbeef.fsvi"),
        b"orphan",
    )?;
    let reuse = root.join(".backfill-reuse-Abandoned123");
    fs::create_dir(&reuse)?;
    fs::write(reuse.join("candidate.fsvi"), b"private snapshot")?;
    Ok(())
}

fn snapshot(data: &Path) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![data.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                pending.push(entry.path());
            } else {
                files.insert(
                    entry.path().strip_prefix(data)?.to_path_buf(),
                    fs::read(entry.path())?,
                );
            }
        }
    }
    Ok(files)
}

fn assert_stale_refusal(data: &Path, input: &mut SemanticManifest) -> Result<()> {
    let before = snapshot(data)?;
    let input_before = input.clone();
    let error = SemanticIndexer::new("hash", None)?
        .run_backfill_batch(
            &rows(9..10),
            data,
            input,
            plan("stale-request", 1, 1, false),
        )
        .expect_err("stale state must not enter the backfill engine");
    assert!(
        error.downcast_ref::<BackfillManifestChanged>().is_some(),
        "{error:#}"
    );
    assert_eq!(
        *input, input_before,
        "do not silently reload a preselected batch's manifest"
    );
    assert_eq!(
        snapshot(data)?,
        before,
        "refusal must precede even scratch reclamation"
    );
    // A refused writer releases its OS lease; recovery is not permanently busy.
    drop(lock_file(&data.join(ARTIFACT_LOCK))?);
    Ok(())
}

#[test]
fn initial_default_cannot_overwrite_a_publication_completed_before_its_lease() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let mut stale = SemanticManifest::default();
    let mut current = SemanticManifest::default();
    let live = publish(data, &mut current)?;
    scratch(data)?;
    assert_stale_refusal(data, &mut stale)?;
    assert_eq!(SemanticManifest::load(data)?.unwrap(), current);
    assert_eq!(VectorIndex::open_read_only(&live)?.record_count(), 3);
    Ok(())
}

#[test]
fn same_path_checkpoint_advance_rejects_the_old_cursor_and_fresh_reload_can_finish() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let indexer = SemanticIndexer::new("hash", None)?;
    let mut manifest = SemanticManifest::default();
    let first = indexer.run_backfill_batch(
        &rows(1..2),
        data,
        &mut manifest,
        plan("resume", 1, 1, false),
    )?;
    let mut stale = manifest.clone();
    let second = indexer.run_backfill_batch(
        &rows(2..3),
        data,
        &mut manifest,
        plan("resume", 2, 1, false),
    )?;
    assert_eq!(first.index_path, second.index_path);
    assert_eq!(manifest.checkpoint.as_ref().unwrap().docs_embedded, 2);
    // The full cursor/count comparison is load-bearing, even within one clock tick.
    manifest.updated_at_ms = stale.updated_at_ms;
    fs::write(SemanticManifest::path(data), serde_json::to_vec(&manifest)?)?;
    scratch(data)?;
    assert_stale_refusal(data, &mut stale)?;

    let mut reloaded = SemanticManifest::load(data)?.unwrap();
    let done =
        indexer.run_backfill_batch(&rows(3..4), data, &mut reloaded, plan("resume", 3, 1, true))?;
    assert!(done.published);
    assert!(!second.index_path.exists());
    assert_eq!(
        VectorIndex::open_read_only(&done.index_path)?.record_count(),
        3
    );
    assert!(plan_backfill_artifacts(data)?.candidates.is_empty());
    Ok(())
}

#[test]
fn published_checkpoint_cannot_be_resurrected_by_an_older_caller() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let indexer = SemanticIndexer::new("hash", None)?;
    let mut manifest = SemanticManifest::default();
    let first = indexer.run_backfill_batch(
        &rows(1..2),
        data,
        &mut manifest,
        plan("resume", 1, 1, false),
    )?;
    let mut stale = manifest.clone();
    let done =
        indexer.run_backfill_batch(&rows(2..4), data, &mut manifest, plan("resume", 3, 2, true))?;
    assert!(done.published && !first.index_path.exists());
    let reader = VectorIndex::open_read_only(&done.index_path)?;
    let query = HashEmbedder::default().embed_sync("compiler recovery lease")?;
    let hits = reader.search_top_k(&query, 3, None)?;
    scratch(data)?;
    assert_stale_refusal(data, &mut stale)?;
    assert!(!first.index_path.exists());
    assert_eq!(reader.search_top_k(&query, 3, None)?, hits);
    assert_eq!(
        VectorIndex::open_read_only(&done.index_path)?.search_top_k(&query, 3, None)?,
        hits
    );
    assert!(SemanticManifest::load(data)?.unwrap().checkpoint.is_none());
    Ok(())
}

#[test]
fn both_tier_records_and_not_ready_accelerators_are_compared_not_just_paths() -> Result<()> {
    for changed in ["fast", "quality", "hnsw"] {
        let temp = tempfile::tempdir()?;
        let data = temp.path();
        let mut current = SemanticManifest::default();
        publish(data, &mut current)?;
        let mut stale = current.clone();
        match changed {
            "fast" => {
                let mut record = current.quality_tier.clone().unwrap();
                record.tier = TierKind::Fast;
                record.ready = false;
                current.fast_tier = Some(record);
            }
            "quality" => {
                let record = current.quality_tier.as_mut().unwrap();
                record.model_revision = "new-producer-revision".into();
                record.ready = false;
            }
            "hnsw" => {
                current.hnsw = Some(HnswRecord {
                    base_tier: TierKind::Quality,
                    embedder_id: "fnv1a-384".into(),
                    ef_search: 16,
                    index_path: "vector_index/selected.chsw".into(),
                    size_bytes: 4,
                    built_at_ms: 1,
                    ready: false,
                });
                fs::write(data.join("vector_index/selected.chsw"), b"keep")?;
            }
            _ => unreachable!(),
        }
        // Equal timestamp and unchanged quality pathname must not hide new authority.
        fs::write(SemanticManifest::path(data), serde_json::to_vec(&current)?)?;
        scratch(data)?;
        assert_stale_refusal(data, &mut stale)?;
    }
    Ok(())
}

#[test]
fn disappearing_manifest_is_not_permission_to_republish_an_old_checkpoint() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let indexer = SemanticIndexer::new("hash", None)?;
    let mut manifest = SemanticManifest::default();
    let checkpoint = indexer.run_backfill_batch(
        &rows(1..2),
        data,
        &mut manifest,
        plan("resume", 1, 1, false),
    )?;
    fs::rename(
        SemanticManifest::path(data),
        data.join("retained-manifest.json"),
    )?;
    scratch(data)?;
    assert_stale_refusal(data, &mut manifest)?;
    assert!(!SemanticManifest::path(data).exists());
    assert!(checkpoint.index_path.is_file());
    Ok(())
}

#[test]
fn unpersisted_checkpoint_edits_do_not_authorize_recovery_after_a_failed_save() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let indexer = SemanticIndexer::new("hash", None)?;
    let mut manifest = SemanticManifest::default();
    indexer.run_backfill_batch(
        &rows(1..2),
        data,
        &mut manifest,
        plan("resume", 1, 1, false),
    )?;
    // Mirrors the in-memory mutation left by an error before manifest rename.
    manifest.checkpoint.as_mut().unwrap().db_fingerprint = "uncommitted-replacement".into();
    scratch(data)?;
    assert_stale_refusal(data, &mut manifest)?;
    let reloaded = SemanticManifest::load(data)?.unwrap();
    drop(BackfillArtifacts::begin(data, &reloaded)?);
    assert!(plan_backfill_artifacts(data)?.candidates.is_empty());
    Ok(())
}

#[test]
fn caller_backlog_estimates_do_not_invalidate_unchanged_artifact_authority() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let mut manifest = SemanticManifest::default();
    publish(data, &mut manifest)?;
    let before = snapshot(data)?;
    manifest.backlog.total_conversations += 100;
    manifest.backlog.db_fingerprint = "new-ingest-estimate".into();
    manifest.backlog.computed_at_ms += 1;
    drop(BackfillArtifacts::begin(data, &manifest)?);
    assert_eq!(snapshot(data)?, before);
    Ok(())
}

#[test]
fn v1_checkpoint_input_is_accepted_without_migrating_or_rewriting_it() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let indexer = SemanticIndexer::new("hash", None)?;
    let mut manifest = SemanticManifest::default();
    indexer.run_backfill_batch(
        &rows(1..2),
        data,
        &mut manifest,
        plan("legacy", 1, 1, false),
    )?;
    manifest.manifest_version = 1;
    manifest.checkpoint.as_mut().unwrap().last_message_id = None;
    manifest.save(data)?;
    let loaded = SemanticManifest::load(data)?.unwrap();
    let before = snapshot(data)?;
    drop(BackfillArtifacts::begin(data, &loaded)?);
    assert_eq!(snapshot(data)?, before);
    Ok(())
}

#[cfg(unix)]
#[test]
fn lease_release_is_explicit_even_when_an_inherited_descriptor_survives() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let path = data.join(ARTIFACT_LOCK);
    fs::write(&path, b"existing owner metadata")?;

    // Live control: this is the previous close-only behavior, with a duplicate
    // standing in for the same open file description inherited across fork.
    let close_only = lock_file(&path)?;
    let inherited = close_only.try_clone()?;
    drop(close_only);
    assert!(lock_file(&path).is_err());
    fs2::FileExt::unlock(&inherited)?;
    drop(inherited);

    let manifest = SemanticManifest::default();
    let owner = BackfillArtifacts::begin(data, &manifest)?;
    let inherited = owner._lock.try_clone()?;
    assert!(BackfillArtifacts::begin(data, &manifest).is_err());
    drop(owner);
    // This must work BEFORE inherited closes, without retries or sleeps.
    let successor = BackfillArtifacts::begin(data, &manifest)?;
    assert!(inherited.metadata()?.is_file());
    assert_eq!(fs::read(&path)?, b"existing owner metadata");
    drop(inherited);
    // Closing the old description cannot unlock a separately admitted owner.
    assert!(BackfillArtifacts::begin(data, &manifest).is_err());
    drop(successor);
    drop(BackfillArtifacts::begin(data, &manifest)?);
    assert_eq!(fs::read(&path)?, b"existing owner metadata");
    Ok(())
}

#[cfg(unix)]
#[test]
fn failed_or_unwound_lease_releases_duplicates_without_reclaiming_scratch() -> Result<()> {
    for unwind in [false, true] {
        let temp = tempfile::tempdir()?;
        let data = temp.path();
        let mut manifest = SemanticManifest::default();
        publish(data, &mut manifest)?;
        let mut inherited = None;
        let mut before_release = None;
        let stopped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
            let owner = BackfillArtifacts::begin(data, &manifest)?;
            inherited = Some(owner._lock.try_clone()?);
            scratch(data)?;
            before_release = Some(snapshot(data)?);
            if unwind {
                panic!("intentional interrupted semantic artifact owner");
            }
            bail!("intentional failed semantic artifact owner")
        }));
        if unwind {
            assert!(stopped.is_err());
        } else {
            assert!(stopped.expect("error return must not unwind").is_err());
        }
        let inherited = inherited.context("the interrupted owner duplicated its descriptor")?;
        let before = before_release.context("the owner recorded state before releasing its lease")?;
        assert_eq!(snapshot(data)?, before, "Drop must not publish or reclaim");
        let successor = lock_file(&data.join(ARTIFACT_LOCK))?;
        assert!(inherited.metadata()?.is_file());
        assert!(
            data.join(VECTOR_INDEX_DIR)
                .join(".backfill-reuse-Abandoned123")
                .is_dir()
        );
        assert_eq!(snapshot(data)?, before, "unlock must not publish or reclaim");
        drop(inherited);
        assert!(lock_file(&data.join(ARTIFACT_LOCK)).is_err());
        drop(successor);
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn inherited_prior_lease_does_not_interrupt_checkpoint_resume_and_publication() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let indexer = SemanticIndexer::new("hash", None)?;
    let mut manifest = SemanticManifest::default();
    let checkpoint = indexer.run_backfill_batch(
        &rows(1..2),
        data,
        &mut manifest,
        plan("resume", 1, 1, false),
    )?;
    let owner = BackfillArtifacts::begin(data, &manifest)?;
    let inherited = owner._lock.try_clone()?;
    let before = snapshot(data)?;
    assert!(
        indexer
            .run_backfill_batch(&rows(2..4), data, &mut manifest, plan("resume", 3, 2, true))
            .is_err(),
        "live ownership must still prevent publication"
    );
    assert_eq!(snapshot(data)?, before);
    drop(owner);

    let complete =
        indexer.run_backfill_batch(&rows(2..4), data, &mut manifest, plan("resume", 3, 2, true))?;
    assert!(complete.published);
    assert!(inherited.metadata()?.is_file(), "the duplicate is still open");
    assert!(!checkpoint.index_path.exists());
    assert!(SemanticManifest::load(data)?.unwrap().checkpoint.is_none());
    let actual = VectorIndex::open_read_only(&complete.index_path)?;
    assert_eq!(actual.record_count(), 3);
    let reference = tempfile::tempdir()?;
    let mut reference_manifest = SemanticManifest::default();
    let expected_path = publish(reference.path(), &mut reference_manifest)?;
    let expected = VectorIndex::open_read_only(&expected_path)?;
    for position in 0..3 {
        assert_eq!(actual.doc_id_at(position)?, expected.doc_id_at(position)?);
        assert_eq!(
            actual.vector_at_f32(position)?,
            expected.vector_at_f32(position)?
        );
    }
    Ok(())
}
