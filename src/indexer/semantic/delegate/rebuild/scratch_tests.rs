//! Rebuild/reconciliation scratch shares backfill's ownership and GC boundary.
//! These tests drive real writers; no embedding models or archive DB are needed.

use super::*;
use crate::indexer::semantic::{
    SemanticBackfillBatchPlan, plan_backfill_artifacts, reclaim_backfill_artifacts,
};
use crate::search::embedder::Embedder;
use crate::search::hash_embedder::HashEmbedder;
use crate::search::semantic_manifest::{HnswRecord, SemanticManifest};
use std::cell::Cell;
use std::fs::OpenOptions;

#[derive(Clone, Copy, Debug)]
enum Operation {
    Full,
    Reconcile,
}

fn fixture(
    data: &Path,
) -> Result<(
    SemanticIndexer,
    Vec<EmbeddedMessage>,
    HashSet<String>,
    PathBuf,
)> {
    let indexer = SemanticIndexer::new("hash", None)?;
    let inputs: Vec<_> = (1..=3)
        .map(|id| EmbeddingInput::new(id, format!("compiler checkpoint evidence {id}")))
        .collect();
    let mut manifest = SemanticManifest::default();
    let live = indexer
        .run_backfill_batch(
            &inputs,
            data,
            &mut manifest,
            SemanticBackfillBatchPlan {
                tier: TierKind::Quality,
                db_fingerprint: "content-v1:rebuild-scratch".into(),
                model_revision: "hash".into(),
                total_conversations: 3,
                conversations_in_batch: 3,
                last_offset: 3,
                cursor_exhausted: true,
            },
        )?
        .index_path;
    let reader = VectorIndex::open_read_only(&live)?;
    let ids = (0..reader.record_count())
        .map(|row| reader.doc_id_at(row).map(str::to_owned))
        .collect::<std::result::Result<HashSet<_>, _>>()?;
    let rows = indexer.embed_messages(&inputs)?;
    Ok((indexer, rows, ids, live))
}

fn scratch_paths(data: &Path) -> Result<Vec<PathBuf>> {
    Ok(fs::read_dir(data.join("vector_index"))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".backfill-reuse-")
        })
        .collect())
}

fn write(
    operation: Operation,
    indexer: &SemanticIndexer,
    rows: Vec<EmbeddedMessage>,
    data: &Path,
    ids: &HashSet<String>,
    check: impl FnMut() -> Result<()>,
) -> Result<VectorIndex> {
    match operation {
        Operation::Full => run(indexer, rows, data, None::<fn(usize)>, check),
        Operation::Reconcile => super::super::reconcile::run(
            indexer,
            rows,
            data,
            TierKind::Quality,
            "content-v1:rebuild-scratch",
            ids,
            check,
        ),
    }
}

#[test]
fn both_public_writers_refuse_an_active_artifact_lease_before_consuming_input() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let (indexer, rows, ids, live) = fixture(data)?;
    let before = fs::read(&live)?;
    let manifest = fs::read(SemanticManifest::path(data))?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(data.join("semantic-backfill-artifacts.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock)?;
    let consumed = Cell::new(false);
    let input = rows.clone().into_iter().inspect(|_| consumed.set(true));
    assert!(indexer.build_and_save_index(input, data).is_err());
    assert!(
        !consumed.get(),
        "a contended rebuild must not start its iterator"
    );
    assert!(
        indexer
            .reconcile_index_with_canonical_documents(
                rows.clone(),
                data,
                TierKind::Quality,
                "content-v1:rebuild-scratch",
                &ids,
            )
            .is_err()
    );
    assert_eq!(fs::read(&live)?, before);
    assert_eq!(fs::read(SemanticManifest::path(data))?, manifest);
    assert!(scratch_paths(data)?.is_empty());
    fs2::FileExt::unlock(&lock)?;
    drop(lock);
    drop(indexer.build_and_save_index(rows, data)?);
    assert!(plan_backfill_artifacts(data)?.candidates.is_empty());
    Ok(())
}

#[test]
fn active_candidates_are_leased_and_failed_writers_release_only_their_own_scratch() -> Result<()> {
    for operation in [Operation::Full, Operation::Reconcile] {
        let temp = tempfile::tempdir()?;
        let data = temp.path();
        let (indexer, rows, ids, live) = fixture(data)?;
        let before = fs::read(&live)?;
        let ledger = fs::read(SemanticManifest::path(data))?;
        let query = HashEmbedder::default().embed_sync("compiler checkpoint")?;
        let reader = VectorIndex::open_read_only(&live)?;
        let hits = reader.search_top_k(&query, 3, None)?;
        let saw_scratch = Cell::new(false);
        let result = write(operation, &indexer, rows.clone(), data, &ids, || {
            let paths = scratch_paths(data)?;
            if !paths.is_empty() {
                assert_eq!(paths.len(), 1);
                assert!(paths[0].is_dir());
                saw_scratch.set(true);
                assert!(plan_backfill_artifacts(data).is_err());
                assert!(reclaim_backfill_artifacts(data).is_err());
                anyhow::bail!("injected interruption before candidate installation");
            }
            Ok(())
        });
        assert!(result.is_err(), "{operation:?}");
        assert!(
            saw_scratch.get(),
            "the actual {operation:?} writer must enter owned scratch"
        );
        assert!(scratch_paths(data)?.is_empty());
        assert_eq!(fs::read(&live)?, before);
        assert_eq!(fs::read(SemanticManifest::path(data))?, ledger);
        assert_eq!(reader.search_top_k(&query, 3, None)?, hits);
        assert_eq!(
            VectorIndex::open_read_only(&live)?.search_top_k(&query, 3, None)?,
            hits
        );
        drop(reader);
        // All candidate owners and the artifact lease must have gone away.
        drop(write(operation, &indexer, rows, data, &ids, || Ok(()))?);
        assert!(scratch_paths(data)?.is_empty());
        assert!(plan_backfill_artifacts(data)?.candidates.is_empty());
    }
    Ok(())
}

#[test]
fn next_rebuild_reclaims_abandoned_work_but_keeps_manifest_owners_and_legacy_directories()
-> Result<()> {
    for operation in [Operation::Full, Operation::Reconcile] {
        let temp = tempfile::tempdir()?;
        let data = temp.path();
        let (indexer, rows, ids, live) = fixture(data)?;
        let root = data.join("vector_index");
        let retained = root.join(".backfill-reuse-Retained123");
        fs::create_dir(&retained)?;
        let protected = retained.join("candidate.fsvi");
        fs::copy(&live, &protected)?;
        let before = fs::read(&protected)?;
        let mut manifest = SemanticManifest::load(data)?.unwrap();
        manifest.hnsw = Some(HnswRecord {
            base_tier: TierKind::Quality,
            embedder_id: "fnv1a-384".into(),
            ef_search: 16,
            index_path: protected.strip_prefix(data)?.to_string_lossy().into_owned(),
            size_bytes: before.len() as u64,
            built_at_ms: 1,
            ready: false,
        });
        manifest.save(data)?;
        let ledger = fs::read(SemanticManifest::path(data))?;
        let abandoned = root.join(".backfill-reuse-Abandoned123");
        fs::create_dir(&abandoned)?;
        fs::write(abandoned.join("candidate.fsvi"), b"interrupted writer")?;
        // Old namespaces did not require the shared lease. Never infer their
        // inactivity or ownership merely from a recognizable-looking name.
        let legacy: Vec<_> = [
            ".semantic-full-rebuild-Old123",
            ".semantic-reconcile-Old123",
        ]
        .into_iter()
        .map(|name| root.join(name))
        .collect();
        for path in &legacy {
            fs::create_dir(path)?;
            fs::write(path.join("candidate.fsvi"), b"unregistered evidence")?;
        }
        drop(write(operation, &indexer, rows, data, &ids, || Ok(()))?);
        assert!(!abandoned.exists());
        assert_eq!(fs::read(&protected)?, before);
        assert_eq!(fs::read(SemanticManifest::path(data))?, ledger);
        for path in &legacy {
            assert_eq!(
                fs::read(path.join("candidate.fsvi"))?,
                b"unregistered evidence"
            );
        }
        assert_eq!(scratch_paths(data)?, vec![retained]);
        assert!(plan_backfill_artifacts(data)?.candidates.is_empty());
    }
    Ok(())
}

#[test]
fn unusable_cleanup_metadata_retains_fallbacks_without_disabling_direct_rebuild_admission()
-> Result<()> {
    for operation in [Operation::Full, Operation::Reconcile] {
        let temp = tempfile::tempdir()?;
        let data = temp.path();
        let (indexer, rows, ids, live) = fixture(data)?;
        let abandoned = data.join("vector_index/.backfill-reuse-Evidence123");
        fs::create_dir(&abandoned)?;
        fs::copy(&live, abandoned.join("candidate.fsvi"))?;
        let before = fs::read(abandoned.join("candidate.fsvi"))?;
        fs::write(SemanticManifest::path(data), b"{unreadable ledger")?;
        // Direct native replacement already has its own strict admission and
        // does not repair this ledger. Only optional scratch cleanup is refused.
        drop(write(operation, &indexer, rows, data, &ids, || Ok(()))?);
        assert_eq!(fs::read(abandoned.join("candidate.fsvi"))?, before);
        assert_eq!(
            fs::read(SemanticManifest::path(data))?,
            b"{unreadable ledger"
        );
        assert_eq!(scratch_paths(data)?, vec![abandoned]);
        assert!(plan_backfill_artifacts(data).is_err());
        assert_eq!(VectorIndex::open_read_only(&live)?.record_count(), 3);
    }
    Ok(())
}

#[test]
fn initial_rebuild_keeps_work_private_and_releases_scratch_on_success_or_unwind() -> Result<()> {
    for unwind in [false, true] {
        let temp = tempfile::tempdir()?;
        let data = temp.path();
        let indexer = SemanticIndexer::new("hash", None)?;
        let rows = indexer.embed_messages(&[EmbeddingInput::new(1, "first private generation")])?;
        let live = vector_index_path(data, indexer.embedder_id());
        let observed = Cell::new(false);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            indexer.build_and_save_index_with_progress(rows, data, |_| {
                observed.set(true);
                assert!(!live.exists());
                assert_eq!(scratch_paths(data).unwrap().len(), 1);
                assert!(plan_backfill_artifacts(data).is_err());
                assert!(!unwind, "injected progress callback unwind");
            })
        }));
        assert!(observed.get());
        if unwind {
            assert!(result.is_err());
            assert!(!live.exists());
        } else {
            drop(result.expect("writer must not unwind")?);
            assert_eq!(VectorIndex::open_read_only(&live)?.record_count(), 1);
        }
        assert!(scratch_paths(data)?.is_empty());
        assert!(plan_backfill_artifacts(data)?.candidates.is_empty());
    }
    Ok(())
}
