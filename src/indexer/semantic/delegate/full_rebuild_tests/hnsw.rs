//! Real graph construction must participate in CASS's artifact ownership lease.
//! All images below are confined to temporary, hash-produced archive fixtures.

use super::*;
use super::super::hnsw::run as run_hnsw;
use crate::indexer::semantic::artifacts::BackfillArtifacts;
use crate::indexer::semantic::{
    SemanticShardBuildPlan, apply_backfill_artifact_plan, plan_backfill_artifacts,
    reclaim_backfill_artifacts,
};
use crate::search::ann_index::{hnsw_index_path, inspect_hnsw_accelerator};
use crate::search::embedder::Embedder;
use crate::search::hash_embedder::HashEmbedder;
use std::collections::BTreeMap;

fn fixture() -> Result<(tempfile::TempDir, SemanticIndexer, PathBuf)> {
    let temp = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    drop(indexer.build_and_save_index(embedded(&indexer)?, temp.path())?);
    let source = vector_index_path(temp.path(), indexer.embedder_id());
    Ok((temp, indexer, source))
}

fn image(root: &Path) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    let mut image = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let relative = path.strip_prefix(root)?;
            if relative == Path::new("index-run.lock")
                || relative == Path::new("semantic-backfill-artifacts.lock")
            {
                continue;
            }
            if entry.file_type()?.is_dir() {
                pending.push(path);
            } else {
                image.insert(relative.to_path_buf(), fs::read(path)?);
            }
        }
    }
    Ok(image)
}

fn assert_busy<T>(result: Result<T>) {
    let error = result.err().expect("active artifact ownership must refuse the operation");
    assert!(format!("{error:#}").contains("owns"), "unexpected refusal: {error:#}");
}

#[test]
fn hnsw_build_and_existing_graph_verification_refuse_a_live_lease_then_retry() -> Result<()> {
    for existing_graph in [false, true] {
        let (temp, indexer, source) = fixture()?;
        let data = temp.path();
        let reader = VectorIndex::open_read_only(&source)?;
        let query = HashEmbedder::default().embed_sync("compiler checkpoint")?;
        let hits = reader.search_top_k(&query, 3, None)?;
        if existing_graph {
            indexer.build_hnsw_index(&reader, data, None, None)?;
        }
        let before = image(data)?;
        let lease = BackfillArtifacts::for_existing_writer(data)?;
        assert_busy(indexer.build_hnsw_index(&reader, data, None, None));
        assert_eq!(image(data)?, before);
        drop(lease);

        let graph = indexer.build_hnsw_index(&reader, data, None, None)?;
        let fresh = VectorIndex::open_read_only(&source)?;
        assert!(inspect_hnsw_accelerator(&graph, &fresh).is_current());
        assert_eq!(reader.search_top_k(&query, 3, None)?, hits);
        assert_eq!(fresh.search_top_k(&query, 3, None)?, hits);
        let published = image(data)?;
        assert_eq!(indexer.build_hnsw_index(&reader, data, None, None)?, graph);
        assert_eq!(image(data)?, published, "native-current graph must remain unchanged");
        drop(BackfillArtifacts::for_existing_writer(data)?);
    }
    Ok(())
}

#[test]
fn hnsw_excludes_recovery_and_public_vector_writers_at_the_native_handoff() -> Result<()> {
    let (temp, indexer, source) = fixture()?;
    let data = temp.path();
    let reader = VectorIndex::open_read_only(&source)?;
    let canonical = (0..reader.record_count())
        .map(|row| Ok(reader.doc_id_at(row)?.to_owned()))
        .collect::<Result<HashSet<_>>>()?;
    let before = image(data)?;
    let mut checks = 0;
    let graph = run_hnsw(&indexer, &reader, data, None, None, || {
        checks += 1;
        if checks == 2 {
            // This fence is immediately before the production native builder.
            assert_busy(plan_backfill_artifacts(data));
            assert_busy(reclaim_backfill_artifacts(data));
            assert_busy(apply_backfill_artifact_plan(data, &"a".repeat(64)));
            assert_busy(indexer.append_to_index(Vec::<EmbeddedMessage>::new(), data));
            assert_busy(indexer.build_and_save_index(Vec::<EmbeddedMessage>::new(), data));
            assert_busy(indexer.build_and_save_index_shards(
                Vec::<EmbeddedMessage>::new(), data, SemanticShardBuildPlan {
                    tier: TierKind::Quality,
                    db_fingerprint: "content-v1:hnsw-lease".into(),
                    model_revision: "hash".into(),
                    total_conversations: 0,
                    max_records_per_shard: 1,
                    build_ann: false,
                },
            ));
            assert_busy(indexer.reconcile_index_with_canonical_documents(
                Vec::new(), data, TierKind::Quality, "content-v1:hnsw-lease", &canonical,
            ));
            assert_busy(indexer.build_hnsw_index(&reader, data, None, None));
            assert_eq!(image(data)?, before);
        }
        Ok(())
    })?;
    assert_eq!(checks, 2);
    assert!(inspect_hnsw_accelerator(&graph, &reader).is_current());
    drop(BackfillArtifacts::for_existing_writer(data)?);
    Ok(())
}

#[test]
fn hnsw_cancellation_and_unwind_release_the_lease_without_replacing_the_graph() -> Result<()> {
    for unwind in [false, true] {
        let (temp, indexer, source) = fixture()?;
        let data = temp.path();
        let reader = VectorIndex::open_read_only(&source)?;
        indexer.build_hnsw_index(&reader, data, None, None)?;
        let before = image(data)?;
        let mut checks = 0;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_hnsw(&indexer, &reader, data, Some(8), Some(32), || {
                checks += 1;
                if checks == 2 {
                    assert_busy(BackfillArtifacts::for_existing_writer(data));
                    assert!(!unwind, "injected pre-publication callback unwind");
                    anyhow::bail!("injected cancellation before native graph work");
                }
                Ok(())
            })
        }));
        assert_eq!(checks, 2);
        if unwind {
            assert!(result.is_err());
        } else {
            assert!(result.expect("cancellation must return normally").is_err());
        }
        assert_eq!(image(data)?, before);
        drop(BackfillArtifacts::for_existing_writer(data)?);
        let graph = indexer.build_hnsw_index(&reader, data, Some(8), Some(32))?;
        assert!(inspect_hnsw_accelerator(&graph, &reader).is_current());
    }
    Ok(())
}

#[test]
fn hnsw_native_contract_failures_release_the_lease_and_preserve_existing_data() -> Result<()> {
    let (temp, indexer, source) = fixture()?;
    let data = temp.path();
    let reader = VectorIndex::open_read_only(&source)?;
    let graph = indexer.build_hnsw_index(&reader, data, None, None)?;
    let foreign_path = data.join("foreign.fsvi");
    let mut writer = VectorIndex::create_with_revision(
        &foreign_path, "other-producer", "other-revision", 4, Quantization::F16,
    )?;
    writer.write_record("foreign", &[1.0, 0.0, 0.0, 0.0])?;
    writer.finish()?;
    let foreign = VectorIndex::open_read_only(&foreign_path)?;
    let before = image(data)?;
    let error = indexer.build_hnsw_index(&foreign, data, None, None).unwrap_err();
    assert!(format!("{error:#}").contains("incompatible semantic vector space"));
    assert_eq!(image(data)?, before);
    assert!(inspect_hnsw_accelerator(&graph, &reader).is_current());
    drop(reader);
    let mut writer = VectorIndex::open_writer(&source)?;
    let vector = writer.vector_at_f32(0)?;
    writer.append("unpublished-WAL-document", &vector)?;
    drop(writer);
    let reader = VectorIndex::open_read_only(&source)?;
    assert_eq!(reader.wal_record_count(), 1);
    let before = image(data)?;
    let error = indexer.build_hnsw_index(&reader, data, None, None).unwrap_err();
    assert!(format!("{error:#}").contains("pending WAL"));
    assert_eq!(image(data)?, before);
    drop(BackfillArtifacts::for_existing_writer(data)?);
    Ok(())
}

#[test]
fn hnsw_lease_does_not_sweep_its_retained_staging_source_or_require_a_valid_ledger() -> Result<()> {
    let (temp, indexer, source) = fixture()?;
    let data = temp.path();
    let staging = data.join("vector_index/.staging-quality-fnv1a-384-deadbeef.fsvi");
    fs::copy(&source, &staging)?;
    let scratch = data.join("vector_index/.backfill-reuse-HnswEvidence123");
    fs::create_dir(&scratch)?;
    fs::write(scratch.join("candidate.fsvi"), b"unrelated recovery evidence")?;
    let ledger = SemanticManifest::path(data);
    fs::write(&ledger, b"{invalid ledger is not graph admission")?;
    let reader = VectorIndex::open_read_only(&staging)?;
    let before = image(data)?;
    let graph = indexer.build_hnsw_index(&reader, data, None, None)?;
    for (path, bytes) in before {
        assert_eq!(fs::read(data.join(path))?, bytes);
    }
    assert!(inspect_hnsw_accelerator(&graph, &reader).is_current());
    assert_eq!(graph, hnsw_index_path(data, indexer.embedder_id()));
    drop(BackfillArtifacts::for_existing_writer(data)?);

    // Building into a new output root from an already retained source remains
    // supported. The lease must not require or create a canonical vector copy.
    let destination = data.join("new-output");
    let graph = indexer.build_hnsw_index(&reader, &destination, None, None)?;
    assert!(inspect_hnsw_accelerator(&graph, &reader).is_current());
    assert!(!vector_index_path(&destination, indexer.embedder_id()).exists());
    assert!(!SemanticManifest::path(&destination).exists());
    Ok(())
}
