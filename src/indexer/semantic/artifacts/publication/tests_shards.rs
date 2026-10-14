//! Ready shard data must survive before any fallback copy may be retired.
//! Exercise the real shard builder and public recovery entry points, including
//! archives that never created a legacy semantic_manifest.json.

use super::*;
use crate::indexer::semantic::{EmbeddingInput, SemanticIndexer, SemanticShardBuildPlan};
use crate::search::embedder::Embedder;
use crate::search::hash_embedder::HashEmbedder;
use crate::search::semantic_manifest::TierKind;
use frankensearch::index::Quantization;
use std::cell::Cell;
use std::collections::BTreeMap;

struct Fixture {
    indexer: SemanticIndexer,
    paths: Vec<PathBuf>,
    fallback: PathBuf,
    reuse: PathBuf,
}

fn optional_bytes(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

impl Fixture {
    fn new(data: &Path, tier: TierKind, with_fallbacks: bool) -> Result<Self> {
        let indexer = SemanticIndexer::new("hash", None)?;
        let inputs = (1..=6)
            .map(|id| EmbeddingInput::new(id, format!("compiler shard recovery evidence {id}")))
            .collect::<Vec<_>>();
        let built = indexer.build_and_save_index_shards(
            indexer.embed_messages(&inputs)?,
            data,
            SemanticShardBuildPlan {
                tier,
                db_fingerprint: "content-v1:shard-recovery".into(),
                model_revision: "hash".into(),
                total_conversations: 6,
                max_records_per_shard: 3,
                build_ann: false,
            },
        )?;
        assert!(built.complete);
        assert_eq!(built.index_paths.len(), 2);
        assert!(!SemanticManifest::path(data).exists());
        let root = data.join(VECTOR_INDEX_DIR);
        let fallback = root.join(".staging-quality-fnv1a-384-deadbeef.fsvi");
        let reuse = root.join(".backfill-reuse-ShardFallback123");
        if with_fallbacks {
            fs::copy(&built.index_paths[1], &fallback)?;
            fs::create_dir(&reuse)?;
            fs::copy(&built.index_paths[1], reuse.join("candidate.fsvi"))?;
        }
        // Offline recovery cannot rely on opening or repairing the database.
        fs::write(data.join("agent_search.db"), b"not a database; do not open")?;
        Ok(Self { indexer, paths: built.index_paths, fallback, reuse })
    }

    fn image(&self, data: &Path) -> Result<BTreeMap<PathBuf, Option<Vec<u8>>>> {
        let mut paths = vec![
            SemanticShardManifest::path(data),
            SemanticManifest::path(data),
            data.join("agent_search.db"),
            self.fallback.clone(),
            self.reuse.join("candidate.fsvi"),
        ];
        for path in &self.paths {
            paths.push(path.clone());
            paths.push(wal_path_for(path));
        }
        paths.into_iter().map(|path| {
            let bytes = optional_bytes(&path)?;
            Ok((path, bytes))
        }).collect()
    }

    fn assert_refused(&self, data: &Path, approval: &str) -> Result<()> {
        let before = self.image(data)?;
        let healthy = VectorIndex::open_read_only(&self.paths[0])?;
        let query = HashEmbedder::default().embed_sync("compiler shard recovery")?;
        let hits = healthy.search_top_k(&query, 3, None)?;
        assert_eq!(hits.len(), 3);
        for result in [
            plan_backfill_artifacts(data).map(|_| ()),
            reclaim_backfill_artifacts(data).map(|_| ()),
            apply_backfill_artifact_plan(data, approval).map(|_| ()),
        ] {
            let error = result.unwrap_err();
            assert!(error.is::<PublicationUnavailable>(), "{error:#}");
        }
        // Cleanup must not disable explicit rebuilding. This closure never
        // publishes, so permitting repair entry is not permission to retire data.
        let mut input = SemanticManifest::default();
        let original = input.clone();
        let entered = Cell::new(false);
        let error = self.indexer.with_backfill_artifacts(data, &mut input, |_, _| {
            entered.set(true);
            assert_eq!(self.image(data)?, before);
            anyhow::bail!("stop at explicit repair entry")
        }).unwrap_err();
        assert!(entered.get(), "repair entry was disabled: {error:#}");
        assert_eq!(input, original);
        let lease = BackfillArtifacts::begin(data, &input)?;
        lease.after_success(&input, &self.paths[0]);
        drop(lease);
        assert_eq!(self.image(data)?, before);
        assert_eq!(healthy.search_top_k(&query, 3, None)?, hits);
        assert_eq!(VectorIndex::open_read_only(&self.paths[0])?.search_top_k(&query, 3, None)?, hits);
        Ok(())
    }
}

#[test]
fn missing_ready_shard_blocks_all_cleanup_without_a_legacy_manifest() -> Result<()> {
    for tier in [TierKind::Fast, TierKind::Quality] {
        let temp = tempfile::tempdir()?;
        let data = temp.path();
        let fixture = Fixture::new(data, tier, true)?;
        let before = fixture.image(data)?;
        let plan = plan_backfill_artifacts(data)?;
        let missing = &fixture.paths[1];
        let retained = data.join("retained-published-shard.fsvi");
        fs::rename(missing, &retained)?;
        fixture.assert_refused(data, &plan.plan_fingerprint)?;
        // Recovery never makes this choice. Restore the fixture explicitly.
        fs::rename(&retained, missing)?;
        let refreshed = plan_backfill_artifacts(data)?;
        let report = apply_backfill_artifact_plan(data, &refreshed.plan_fingerprint)?;
        assert_eq!((report.removed_files, report.removed_directories), (1, 1));
        assert!(report.failed_paths.is_empty());
        for path in &fixture.paths {
            assert_eq!(optional_bytes(path)?, before[path]);
        }
        assert_eq!(optional_bytes(&SemanticShardManifest::path(data))?, before[&SemanticShardManifest::path(data)]);
        assert!(!fixture.fallback.exists() && !fixture.reuse.exists());
        assert!(!SemanticManifest::path(data).exists());
    }
    Ok(())
}

#[test]
fn ready_shard_damage_or_wrong_space_cannot_retire_fallbacks() -> Result<()> {
    for damage in 0..4 {
        let temp = tempfile::tempdir()?;
        let data = temp.path();
        let fixture = Fixture::new(data, TierKind::Quality, true)?;
        let plan = plan_backfill_artifacts(data)?;
        let path = &fixture.paths[1];
        match damage {
            0 => fs::write(path, b"truncated shard")?,
            1 | 2 => {
                let dimension = if damage == 2 { 8 } else { 384 };
                let producer = if damage == 1 { "foreign" } else { "fnv1a-384" };
                let mut writer = VectorIndex::create_with_revision(
                    path, producer, "fixture", dimension, Quantization::F16,
                )?;
                for id in ["a", "b", "c"] {
                    writer.write_record(id, &vec![1.0; dimension])?;
                }
                writer.finish()?;
            }
            _ => {
                let mut writer = VectorIndex::open(path)?;
                let id = writer.doc_id_at(0)?.to_owned();
                assert!(writer.soft_delete(&id)?);
            }
        }
        fixture.assert_refused(data, &plan.plan_fingerprint)?;
    }
    Ok(())
}

#[test]
fn lost_acknowledged_shard_wal_keeps_fallbacks_until_explicit_restoration() -> Result<()> {
    for torn in [false, true] {
        let temp = tempfile::tempdir()?;
        let data = temp.path();
        let fixture = Fixture::new(data, TierKind::Quality, true)?;
        let path = &fixture.paths[1];
        let mut writer = VectorIndex::open(path)?;
        let id = writer.doc_id_at(0)?.to_owned();
        let vector = writer.vector_at_f32(0)?;
        assert!(writer.soft_delete(&id)?);
        writer.append(&id, &vector)?;
        drop(writer);
        let plan = plan_backfill_artifacts(data)?;
        let wal = wal_path_for(path);
        let original = fs::read(&wal)?;
        if torn {
            fs::write(&wal, &original[..original.len() - 1])?;
        } else {
            fs::rename(&wal, data.join("retained-shard.wal"))?;
        }
        fixture.assert_refused(data, &plan.plan_fingerprint)?;
        fs::write(&wal, &original)?;
        let refreshed = plan_backfill_artifacts(data)?;
        let report = apply_backfill_artifact_plan(data, &refreshed.plan_fingerprint)?;
        assert_eq!((report.removed_files, report.removed_directories), (1, 1));
        assert_eq!(fs::read(&wal)?, original, "GC must not compact or repair the WAL");
    }
    Ok(())
}

#[test]
fn healthy_shard_wal_is_pinned_and_failed_sync_is_a_typed_refusal() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let fixture = Fixture::new(data, TierKind::Quality, true)?;
    let path = &fixture.paths[1];
    let mut writer = VectorIndex::open(path)?;
    let vector = writer.vector_at_f32(0)?;
    writer.append("additional-shard-document", &vector)?;
    drop(writer);
    let manifest = SemanticShardManifest::load(data)?.unwrap();
    let before = fixture.image(data)?;
    let mut pins = Vec::new();
    validate_shards_with(data, &manifest, |path, main, wal_path, wal| {
        assert!(main.metadata()?.is_file());
        if let Some(wal) = wal {
            assert_eq!(wal.metadata()?.len(), fs::metadata(wal_path)?.len());
        }
        pins.push((path.to_path_buf(), wal.is_some()));
        Ok(())
    })?;
    assert_eq!(pins.len(), 2);
    assert_eq!(pins[1], (path.clone(), true));
    let error = validate_shards_with(data, &manifest, |_, _, _, _| {
        Err(io::Error::other("injected shard durability failure").into())
    }).unwrap_err();
    assert!(error.is::<PublicationUnavailable>());
    assert_eq!(fixture.image(data)?, before);
    let plan = plan_backfill_artifacts(data)?;
    apply_backfill_artifact_plan(data, &plan.plan_fingerprint)?;
    for path in fixture.paths.iter().flat_map(|path| [path.clone(), wal_path_for(path)]) {
        assert_eq!(optional_bytes(&path)?, before[&path]);
    }
    Ok(())
}

#[test]
fn not_ready_shards_keep_ownership_without_requiring_valid_data_or_an_ann() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let _fixture = Fixture::new(data, TierKind::Quality, true)?;
    let protected_dir = data.join("vector_index/.backfill-reuse-OwnedShard123");
    fs::create_dir(&protected_dir)?;
    let protected = protected_dir.join("candidate.fsvi");
    fs::write(&protected, b"not-ready shard evidence")?;
    let mut manifest = SemanticShardManifest::load(data)?.unwrap();
    manifest.shards[1].index_path = protected.strip_prefix(data)?.to_string_lossy().into_owned();
    manifest.shards[1].ready = false;
    // Optional accelerator absence is not missing acknowledged vector data.
    manifest.shards[0].ann_index_path = Some("vector_index/missing-optional.chsw".into());
    manifest.shards[0].ann_ready = true;
    manifest.save(data)?;
    let before = fs::read(SemanticShardManifest::path(data))?;
    let plan = plan_backfill_artifacts(data)?;
    assert_eq!((plan.reclaimable_files, plan.reclaimable_directories), (1, 1));
    apply_backfill_artifact_plan(data, &plan.plan_fingerprint)?;
    assert_eq!(fs::read(&protected)?, b"not-ready shard evidence");
    assert_eq!(fs::read(SemanticShardManifest::path(data))?, before);
    Ok(())
}

#[test]
fn shard_metadata_errors_are_not_downgraded_to_repairable_data_failures() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let fixture = Fixture::new(data, TierKind::Quality, true)?;
    let path = SemanticShardManifest::path(data);
    for bytes in [
        b"{broken shard ledger".to_vec(),
        serde_json::to_vec(&SemanticShardManifest {
            manifest_version: u32::MAX,
            ..SemanticShardManifest::default()
        })?,
    ] {
        fs::write(&path, &bytes)?;
        let error = plan_backfill_artifacts(data).unwrap_err();
        assert!(!error.is::<PublicationUnavailable>());
        let entered = Cell::new(false);
        let mut input = SemanticManifest::default();
        assert!(fixture.indexer.with_backfill_artifacts(data, &mut input, |_, _| {
            entered.set(true);
            anyhow::bail!("must never reach repair with invalid authority")
        }).is_err());
        assert!(!entered.get());
        assert_eq!(fs::read(&path)?, bytes);
        assert!(fixture.fallback.is_file() && fixture.reuse.is_dir());
    }
    Ok(())
}

#[test]
fn no_candidates_do_not_trigger_ready_shard_data_admission() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let fixture = Fixture::new(data, TierKind::Quality, false)?;
    fs::rename(&fixture.paths[1], data.join("retained-published-shard.fsvi"))?;
    let plan = plan_backfill_artifacts(data)?;
    assert!(plan.candidates.is_empty());
    assert_eq!(plan.reclaimable_bytes, 0);
    assert!(!SemanticManifest::path(data).exists());
    Ok(())
}

#[test]
fn enough_shard_ids_with_unusable_persisted_signal_still_retain_fallbacks() -> Result<()> {
    for tier in [TierKind::Fast, TierKind::Quality] {
        for value in [1.0e-8_f32, 70_000.0_f32] {
            let temp = tempfile::tempdir()?;
            let data = temp.path();
            let fixture = Fixture::new(data, tier, true)?;
            let approval = plan_backfill_artifacts(data)?;
            let path = &fixture.paths[1];
            let reader = VectorIndex::open_read_only(path)?;
            let ids = (0..reader.record_count())
                .map(|row| Ok(reader.doc_id_at(row)?.to_owned()))
                .collect::<Result<Vec<_>>>()?;
            drop(reader);
            let mut writer = VectorIndex::create_with_revision(
                path,
                "fnv1a-384",
                crate::indexer::semantic::HASH_VECTOR_SPACE_REVISION,
                384,
                Quantization::F16,
            )?;
            for (position, id) in ids.iter().enumerate() {
                let mut vector = vec![0.0; 384];
                vector[0] = if position == 1 { value } else { 1.0 };
                writer.write_record(id, &vector)?;
            }
            writer.finish()?;
            let reader = VectorIndex::open_read_only(path)?;
            checkpoint_coverage::require_distinct_live(&reader, 3)?;
            let bad_row = (0..reader.record_count())
                .find(|&row| reader.doc_id_at(row).unwrap() == ids[1])
                .expect("the damaged vector must retain its exact document ID");
            assert!(!reader.is_vector_usable(bad_row));
            drop(reader);
            let manifest = SemanticShardManifest::load(data)?.unwrap();
            let error = validate_shards_with(data, &manifest, |seen, _, _, _| {
                assert_ne!(seen, path, "unusable shard cannot reach durability admission");
                Ok(())
            }).unwrap_err();
            assert!(error.is::<PublicationUnavailable>());
            assert!(format!("{error:#}").contains("unusable live vector"));
            fixture.assert_refused(data, &approval.plan_fingerprint)?;
        }
    }
    Ok(())
}
