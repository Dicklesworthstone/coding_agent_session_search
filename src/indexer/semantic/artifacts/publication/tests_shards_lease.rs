//! Shard-sidecar publication must participate in recovery's artifact lease.
//! All sources, metadata and interrupted evidence below are temporary fixtures.

use super::*;
use crate::indexer::semantic::{
    EmbeddedMessage, EmbeddingInput, SemanticBackfillBatchPlan, SemanticIndexer,
    SemanticShardBuildPlan,
};
use crate::search::embedder::Embedder;
use crate::search::hash_embedder::HashEmbedder;
use crate::search::semantic_manifest::TierKind;
use std::cell::Cell;
use std::collections::{BTreeMap, HashSet};

fn plan(tier: TierKind, fingerprint: &str) -> SemanticShardBuildPlan {
    SemanticShardBuildPlan {
        tier,
        db_fingerprint: fingerprint.into(),
        model_revision: "hash".into(),
        total_conversations: 3,
        max_records_per_shard: 1,
        build_ann: false,
    }
}

fn rows(indexer: &SemanticIndexer) -> Result<Vec<EmbeddedMessage>> {
    let inputs = (1..=3)
        .map(|id| EmbeddingInput::new(id, format!("shard lease publication document {id}")))
        .collect::<Vec<_>>();
    indexer.embed_messages(&inputs)
}

fn seed(data: &Path) -> Result<(SemanticIndexer, Vec<PathBuf>)> {
    let indexer = SemanticIndexer::new("hash", None)?;
    let built = indexer.build_and_save_index_shards(
        rows(&indexer)?,
        data,
        plan(TierKind::Fast, "content-v1:lease-fast"),
    )?;
    assert!(built.complete);
    assert_eq!(built.index_paths.len(), 3);
    Ok((indexer, built.index_paths))
}

fn image(paths: &[PathBuf]) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    paths
        .iter()
        .map(|path| Ok((path.clone(), fs::read(path)?)))
        .collect()
}

fn scratch(data: &Path) -> Result<PathBuf> {
    let directory = data.join("vector_index/.backfill-reuse-Independent123");
    fs::create_dir(&directory)?;
    let evidence = directory.join("candidate.fsvi");
    fs::write(&evidence, b"unrelated interrupted evidence")?;
    Ok(evidence)
}

fn assert_busy<T>(result: Result<T>) {
    let error = result
        .err()
        .expect("an active shard publisher must exclude this operation");
    assert!(
        format!("{error:#}").contains("owns"),
        "unexpected refusal: {error:#}"
    );
}

#[test]
fn contended_shard_build_preserves_the_manifest_and_retry_keeps_both_tiers() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let (indexer, published) = seed(data)?;
    let before = image(&published)?;
    let manifest = SemanticShardManifest::load(data)?.unwrap();
    let ledger = fs::read(SemanticShardManifest::path(data))?;
    let reader = VectorIndex::open_read_only(&published[0])?;
    let query = HashEmbedder::default().embed_sync("shard lease publication")?;
    let hits = reader.search_top_k(&query, 3, None)?;
    let lease = BackfillArtifacts::for_existing_writer(data)?;
    let consumed = Cell::new(false);
    let input = rows(&indexer)?.into_iter().inspect(|_| consumed.set(true));
    assert_busy(indexer.build_and_save_index_shards(
        input,
        data,
        plan(TierKind::Quality, "content-v1:lease-quality"),
    ));
    assert!(!consumed.get());
    assert_eq!(fs::read(SemanticShardManifest::path(data))?, ledger);
    assert_eq!(image(&published)?, before);
    drop(lease);
    let built = indexer.build_and_save_index_shards(
        rows(&indexer)?,
        data,
        plan(TierKind::Quality, "content-v1:lease-quality"),
    )?;
    assert!(built.complete);
    let after = SemanticShardManifest::load(data)?.unwrap();
    assert_eq!(after.shards.len(), manifest.shards.len() + 3);
    for record in &manifest.shards {
        assert!(
            after.shards.contains(record),
            "another tier's authority was replaced"
        );
    }
    assert_eq!(image(&published)?, before);
    assert_eq!(reader.search_top_k(&query, 3, None)?, hits);
    assert_eq!(
        VectorIndex::open_read_only(&published[0])?.search_top_k(&query, 3, None)?,
        hits
    );
    drop(BackfillArtifacts::for_existing_writer(data)?);
    Ok(())
}

#[test]
fn active_shard_build_excludes_recovery_and_other_public_vector_writers() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let (indexer, published) = seed(data)?;
    let before = image(&published)?;
    let ledger = fs::read(SemanticShardManifest::path(data))?;
    let evidence = scratch(data)?;
    let mut input = rows(&indexer)?.into_iter();
    let calls = Cell::new(0usize);
    let checked = std::iter::from_fn(|| {
        calls.set(calls.get() + 1);
        // This includes final iterator exhaustion, before sidecar publication.
        assert_busy(plan_backfill_artifacts(data));
        assert_busy(reclaim_backfill_artifacts(data));
        assert_busy(indexer.build_and_save_index_shards(
            Vec::<EmbeddedMessage>::new(),
            data,
            plan(TierKind::Quality, "content-v1:other"),
        ));
        assert_busy(indexer.build_and_save_index(Vec::<EmbeddedMessage>::new(), data));
        assert_busy(indexer.append_to_index(Vec::<EmbeddedMessage>::new(), data));
        assert_busy(indexer.reconcile_index_with_canonical_documents(
            Vec::new(),
            data,
            TierKind::Quality,
            "content-v1:other",
            &HashSet::new(),
        ));
        assert_busy(indexer.run_backfill_batch(
            &[],
            data,
            &mut SemanticManifest::default(),
            SemanticBackfillBatchPlan {
                tier: TierKind::Quality,
                db_fingerprint: "content-v1:other".into(),
                model_revision: "hash".into(),
                total_conversations: 0,
                conversations_in_batch: 0,
                last_offset: 0,
                cursor_exhausted: true,
            },
        ));
        assert_eq!(fs::read(SemanticShardManifest::path(data)).unwrap(), ledger);
        assert_eq!(image(&published).unwrap(), before);
        input.next()
    });
    let built = indexer.build_and_save_index_shards(
        checked,
        data,
        plan(TierKind::Quality, "content-v1:lease-quality"),
    )?;
    assert!(built.complete);
    assert_eq!(calls.get(), 4);
    assert_eq!(image(&published)?, before);
    assert_eq!(fs::read(&evidence)?, b"unrelated interrupted evidence");
    // Publication itself does not sweep; a later explicit preview owns admission.
    assert_eq!(plan_backfill_artifacts(data)?.reclaimable_directories, 1);
    Ok(())
}

#[test]
fn shard_errors_and_iterator_unwinds_release_the_lease_without_replacing_authority() -> Result<()> {
    for unwind in [false, true] {
        let temp = tempfile::tempdir()?;
        let data = temp.path();
        let (indexer, published) = seed(data)?;
        let before = image(&published)?;
        let ledger = fs::read(SemanticShardManifest::path(data))?;
        let evidence = scratch(data)?;
        let mut replacement = rows(&indexer)?;
        if !unwind {
            replacement[1].embedding[0] = f32::NAN;
        }
        let visited = Cell::new(0usize);
        let input = replacement.into_iter().inspect(|_| {
            visited.set(visited.get() + 1);
            assert_busy(BackfillArtifacts::for_existing_writer(data));
            assert!(
                !(unwind && visited.get() == 2),
                "injected shard iterator unwind"
            );
        });
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            indexer.build_and_save_index_shards(
                input,
                data,
                plan(TierKind::Quality, "content-v1:failed"),
            )
        }));
        assert_eq!(visited.get(), 2);
        if unwind {
            assert!(result.is_err());
        } else {
            assert!(
                result
                    .expect("only the deliberate iterator unwind may panic")
                    .is_err()
            );
        }
        assert_eq!(fs::read(SemanticShardManifest::path(data))?, ledger);
        assert_eq!(image(&published)?, before);
        assert_eq!(fs::read(&evidence)?, b"unrelated interrupted evidence");
        drop(BackfillArtifacts::for_existing_writer(data)?);
        assert!(
            indexer
                .build_and_save_index_shards(
                    rows(&indexer)?,
                    data,
                    plan(TierKind::Quality, "content-v1:retry"),
                )?
                .complete
        );
        assert_eq!(image(&published)?, before);
    }
    Ok(())
}

#[test]
fn invalid_shard_plans_do_not_consume_input_or_create_an_archive() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    for blank_fingerprint in [false, true] {
        let data = temp
            .path()
            .join(if blank_fingerprint { "blank" } else { "zero" });
        let mut request = plan(TierKind::Quality, "content-v1:invalid");
        if blank_fingerprint {
            request.db_fingerprint = "  ".into();
        } else {
            request.max_records_per_shard = 0;
        }
        let consumed = Cell::new(false);
        let input = rows(&indexer)?.into_iter().inspect(|_| consumed.set(true));
        assert!(
            indexer
                .build_and_save_index_shards(input, &data, request)
                .is_err()
        );
        assert!(!consumed.get());
        assert!(!data.exists());
    }
    Ok(())
}

#[test]
fn shard_publisher_does_not_replace_invalid_metadata_or_reclaim_scratch() -> Result<()> {
    for future in [false, true] {
        let temp = tempfile::tempdir()?;
        let data = temp.path();
        let (indexer, published) = seed(data)?;
        let before = image(&published)?;
        let evidence = scratch(data)?;
        let bytes = if future {
            serde_json::to_vec(&SemanticShardManifest {
                manifest_version: u32::MAX,
                ..SemanticShardManifest::default()
            })?
        } else {
            b"{invalid shard sidecar".to_vec()
        };
        fs::write(SemanticShardManifest::path(data), &bytes)?;
        let consumed = Cell::new(false);
        let input = rows(&indexer)?.into_iter().inspect(|_| consumed.set(true));
        assert!(
            indexer
                .build_and_save_index_shards(
                    input,
                    data,
                    plan(TierKind::Quality, "content-v1:refused"),
                )
                .is_err()
        );
        assert!(!consumed.get());
        assert_eq!(fs::read(SemanticShardManifest::path(data))?, bytes);
        assert_eq!(image(&published)?, before);
        assert_eq!(fs::read(&evidence)?, b"unrelated interrupted evidence");
        drop(BackfillArtifacts::for_existing_writer(data)?);
    }
    Ok(())
}

#[test]
fn shard_publication_keeps_not_ready_references_without_data_admission_or_cleanup() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let (indexer, published) = seed(data)?;
    let before = image(&published)?;
    let evidence = scratch(data)?;
    let mut old = SemanticShardManifest::load(data)?.unwrap();
    old.shards[0].ready = false;
    old.shards[0].index_path = evidence.strip_prefix(data)?.to_string_lossy().into_owned();
    old.save(data)?;
    let built = indexer.build_and_save_index_shards(
        rows(&indexer)?,
        data,
        plan(TierKind::Quality, "content-v1:new-tier"),
    )?;
    assert!(built.complete);
    let after = SemanticShardManifest::load(data)?.unwrap();
    for record in &old.shards {
        assert!(after.shards.contains(record));
    }
    assert_eq!(image(&published)?, before);
    assert_eq!(fs::read(&evidence)?, b"unrelated interrupted evidence");
    drop(BackfillArtifacts::for_existing_writer(data)?);
    Ok(())
}
