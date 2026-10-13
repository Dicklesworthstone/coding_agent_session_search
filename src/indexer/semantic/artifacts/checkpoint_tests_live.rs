//! Regression for acknowledged coverage hidden by physical tombstoned slots.
//! Use production legacy mutation/WAL APIs, not a v2-only tombstone writer.

use super::*;
use crate::indexer::semantic::{EmbeddingInput, SemanticBackfillBatchPlan, SemanticIndexer};
use crate::search::embedder::Embedder;
use crate::search::hash_embedder::HashEmbedder;
use crate::search::semantic_manifest::TierKind;
use frankensearch::index::VectorIndex;

fn batch(tier: TierKind, fingerprint: &str, complete: bool) -> SemanticBackfillBatchPlan {
    SemanticBackfillBatchPlan {
        tier,
        db_fingerprint: fingerprint.into(),
        model_revision: "hash".into(),
        total_conversations: if complete { 3 } else { 4 },
        conversations_in_batch: 3,
        last_offset: 3,
        cursor_exhausted: complete,
    }
}

struct Fixture {
    indexer: SemanticIndexer,
    manifest: SemanticManifest,
    live: PathBuf,
    checkpoint: PathBuf,
    fallbacks: Vec<PathBuf>,
}

impl Fixture {
    fn new(data: &Path, tier: TierKind) -> Result<Self> {
        let indexer = SemanticIndexer::new("hash", None)?;
        let mut manifest = SemanticManifest::default();
        let rows: Vec<_> = (1..=3)
            .map(|id| EmbeddingInput::new(id, format!("compiler recovery document {id}")))
            .collect();
        let live = indexer
            .run_backfill_batch(
                &rows,
                data,
                &mut manifest,
                batch(TierKind::Quality, "content-v1:live", true),
            )?
            .index_path;
        let checkpoint = indexer
            .run_backfill_batch(
                &rows,
                data,
                &mut manifest,
                batch(tier, "content-v1:resume", false),
            )?
            .index_path;
        assert_eq!(manifest.checkpoint.as_ref().unwrap().docs_embedded, 3);
        let root = data.join(VECTOR_INDEX_DIR);
        let fallback = root.join(".staging-quality-fnv1a-384-deadbeef.fsvi");
        fs::copy(&checkpoint, &fallback)?;
        let reuse = root.join(".backfill-reuse-LiveCoverage123");
        fs::create_dir(&reuse)?;
        let reused = reuse.join("candidate.fsvi");
        fs::copy(&checkpoint, &reused)?;
        Ok(Self {
            indexer,
            manifest,
            live,
            checkpoint,
            fallbacks: vec![fallback, reused],
        })
    }

    fn snapshot(&self, data: &Path) -> Result<Vec<(PathBuf, Vec<u8>)>> {
        self.fallbacks
            .iter()
            .cloned()
            .chain([
                self.live.clone(),
                self.checkpoint.clone(),
                SemanticManifest::path(data),
            ])
            .map(|path| Ok((path.clone(), fs::read(path)?)))
            .collect()
    }

    fn tombstone(&self, replace_in_wal: bool) -> Result<()> {
        let mut writer = VectorIndex::open(&self.checkpoint)?;
        let id = writer.doc_id_at(0)?.to_owned();
        let vector = writer.vector_at_f32(0)?;
        assert!(writer.soft_delete(&id)?);
        if replace_in_wal {
            writer.append(&id, &vector)?;
        }
        drop(writer);
        let reader = VectorIndex::open_read_only(&self.checkpoint)?;
        assert_eq!(reader.record_count(), 3);
        assert_eq!(reader.tombstone_count(), 1);
        assert_eq!(reader.wal_record_count(), usize::from(replace_in_wal));
        Ok(())
    }

    fn assert_refused(
        &mut self,
        data: &Path,
        approval: &BackfillArtifactReclaimPlan,
    ) -> Result<()> {
        let before = self.snapshot(data)?;
        let input = self.manifest.clone();
        let wal = wal_path_for(&self.checkpoint);
        let wal_before = fs::read(&wal).ok();
        let reader = VectorIndex::open_read_only(&self.live)?;
        let query = HashEmbedder::default().embed_sync("compiler recovery document")?;
        let hits = reader.search_top_k(&query, 3, None)?;
        assert_eq!(hits.len(), 3);

        let error = reclaim_backfill_artifacts(data).unwrap_err();
        assert!(format!("{error:#}").contains("retaining fallback artifacts"));
        assert!(plan_backfill_artifacts(data).is_err());
        assert!(apply_backfill_artifact_plan(data, &approval.plan_fingerprint).is_err());
        assert!(
            self.indexer
                .run_backfill_batch(
                    &[EmbeddingInput::new(4, "not allowed to start another batch")],
                    data,
                    &mut self.manifest,
                    batch(TierKind::Quality, "content-v1:new", false),
                )
                .is_err()
        );
        // The post-success path must defer cleanup too, not just explicit GC.
        let artifacts = BackfillArtifacts::lock(data)?;
        artifacts.after_success(&self.manifest, &self.checkpoint);
        drop(artifacts);

        assert_eq!(self.manifest, input);
        for (path, bytes) in before {
            assert_eq!(fs::read(&path)?, bytes, "changed {}", path.display());
        }
        assert_eq!(fs::read(&wal).ok(), wal_before, "GC must not repair the WAL");
        assert_eq!(reader.search_top_k(&query, 3, None)?, hits);
        assert_eq!(
            VectorIndex::open_read_only(&self.live)?.search_top_k(&query, 3, None)?,
            hits
        );
        Ok(())
    }
}

#[test]
fn tombstoned_main_slots_cannot_authorize_retiring_acknowledged_fallbacks() -> Result<()> {
    for tier in [TierKind::Fast, TierKind::Quality] {
        let temp = tempfile::tempdir()?;
        let data = temp.path();
        let mut fixture = Fixture::new(data, tier)?;
        let approval = plan_backfill_artifacts(data)?;
        fixture.tombstone(false)?;
        assert!(!wal_path_for(&fixture.checkpoint).exists());
        fixture.assert_refused(data, &approval)?;
    }
    Ok(())
}

#[test]
fn missing_or_torn_replacement_wal_cannot_hide_behind_its_tombstoned_main_slot() -> Result<()> {
    for missing in [false, true] {
        let temp = tempfile::tempdir()?;
        let data = temp.path();
        let mut fixture = Fixture::new(data, TierKind::Quality)?;
        fixture.tombstone(true)?;
        // A complete replacement WAL really does cover the acknowledged row.
        let approval = plan_backfill_artifacts(data)?;
        let wal = wal_path_for(&fixture.checkpoint);
        let healthy_wal = fs::read(&wal)?;
        if missing {
            fs::rename(&wal, data.join("retained-original.wal"))?;
        } else {
            fs::write(&wal, &healthy_wal[..healthy_wal.len() - 1])?;
        }
        let reader = VectorIndex::open_read_only(&fixture.checkpoint)?;
        assert_eq!(reader.record_count(), 3, "the old physical bound still passes");
        assert_eq!(reader.tombstone_count(), 1);
        assert_eq!(reader.wal_record_count(), 0);
        drop(reader);
        fixture.assert_refused(data, &approval)?;

        // Only explicit fixture restoration permits a new approval. The GC
        // does not select an orphan, reconstruct data, or rewrite the ledger.
        fs::write(&wal, &healthy_wal)?;
        let refreshed = plan_backfill_artifacts(data)?;
        let report = apply_backfill_artifact_plan(data, &refreshed.plan_fingerprint)?;
        assert_eq!((report.removed_files, report.removed_directories), (1, 1));
        assert!(report.failed_paths.is_empty());
        assert_eq!(fs::read(&wal)?, healthy_wal);
        for path in &fixture.fallbacks {
            assert!(!path.exists());
        }
    }
    Ok(())
}

#[test]
fn tombstones_with_sufficient_live_main_or_replayable_wal_coverage_remain_reclaimable() -> Result<()> {
    for replace_in_wal in [false, true] {
        let temp = tempfile::tempdir()?;
        let data = temp.path();
        let mut fixture = Fixture::new(data, TierKind::Quality)?;
        fixture.tombstone(replace_in_wal)?;
        if !replace_in_wal {
            fixture.manifest.checkpoint.as_mut().unwrap().docs_embedded = 2;
            fixture.manifest.save(data)?;
        }
        let before = fixture.snapshot(data)?;
        let wal = wal_path_for(&fixture.checkpoint);
        let wal_before = fs::read(&wal).ok();
        let reader = VectorIndex::open_read_only(&fixture.checkpoint)?;
        let query = HashEmbedder::default().embed_sync("compiler recovery document")?;
        let hits = reader.search_top_k(&query, 10, None)?;
        assert_eq!(hits.len(), if replace_in_wal { 3 } else { 2 });
        let approval = plan_backfill_artifacts(data)?;
        let report = apply_backfill_artifact_plan(data, &approval.plan_fingerprint)?;
        assert_eq!((report.removed_files, report.removed_directories), (1, 1));
        assert!(report.failed_paths.is_empty());
        for (path, bytes) in before {
            if !fixture.fallbacks.contains(&path) {
                assert_eq!(fs::read(&path)?, bytes);
            }
        }
        assert_eq!(fs::read(&wal).ok(), wal_before);
        assert_eq!(reader.search_top_k(&query, 10, None)?, hits);
        assert_eq!(
            VectorIndex::open_read_only(&fixture.checkpoint)?.search_top_k(&query, 10, None)?,
            hits
        );
    }
    Ok(())
}

mod overlap;
