//! Reclamation must not mistake a durable ready record for durable vector data.
//! All mutations below are confined to fresh, real hash-produced FSVI fixtures.

use super::*;
use crate::indexer::semantic::{EmbeddingInput, SemanticBackfillBatchPlan, SemanticIndexer};
use crate::search::embedder::Embedder;
use crate::search::hash_embedder::HashEmbedder;
use crate::search::semantic_manifest::TierKind;
use frankensearch::index::Quantization;

fn plan(tier: TierKind, fingerprint: &str) -> SemanticBackfillBatchPlan {
    SemanticBackfillBatchPlan {
        tier,
        db_fingerprint: fingerprint.into(),
        model_revision: "hash".into(),
        total_conversations: 3,
        conversations_in_batch: 3,
        last_offset: 3,
        cursor_exhausted: true,
    }
}

fn rows() -> Vec<EmbeddingInput> {
    (1..=3)
        .map(|id| EmbeddingInput::new(id, format!("compiler publication recovery {id}")))
        .collect()
}

struct Fixture {
    indexer: SemanticIndexer,
    manifest: SemanticManifest,
    live: PathBuf,
    staging: PathBuf,
    reuse: PathBuf,
}

impl Fixture {
    fn new(data: &Path, tier: TierKind) -> Result<Self> {
        let indexer = SemanticIndexer::new("hash", None)?;
        let mut manifest = SemanticManifest::default();
        let live = indexer
            .run_backfill_batch(&rows(), data, &mut manifest, plan(tier, "content-v1:live"))?
            .index_path;
        assert!(manifest.checkpoint.is_none());
        let root = data.join(VECTOR_INDEX_DIR);
        let staging = root.join(".staging-quality-fnv1a-384-deadbeef.fsvi");
        fs::copy(&live, &staging)?;
        let reuse = root.join(".backfill-reuse-Publication123");
        fs::create_dir(&reuse)?;
        fs::copy(&live, reuse.join("candidate.fsvi"))?;
        Ok(Self {
            indexer,
            manifest,
            live,
            staging,
            reuse,
        })
    }

    fn watched(&self, data: &Path) -> Vec<PathBuf> {
        vec![
            self.live.clone(),
            wal_path_for(&self.live),
            self.staging.clone(),
            self.reuse.join("candidate.fsvi"),
            SemanticManifest::path(data),
        ]
    }

    fn assert_retained(
        &mut self,
        data: &Path,
        approval: &BackfillArtifactReclaimPlan,
    ) -> Result<()> {
        let before = snapshot(&self.watched(data))?;
        let input = self.manifest.clone();
        for result in [
            reclaim_backfill_artifacts(data).map(|_| ()),
            plan_backfill_artifacts(data).map(|_| ()),
            apply_backfill_artifact_plan(data, &approval.plan_fingerprint).map(|_| ()),
        ] {
            let error = result.unwrap_err();
            assert!(
                error.is::<PublicationUnavailable>(),
                "wrong refusal: {error:#}"
            );
        }
        // Post-success GC has the same admission, not a less conservative path.
        let artifacts = BackfillArtifacts::lock(data)?;
        artifacts.after_success(&self.manifest, &self.live);
        drop(artifacts);
        let mut entered = false;
        let error = self
            .indexer
            .with_backfill_artifacts(data, &mut self.manifest, |_, _| {
                entered = true;
                assert_eq!(
                    snapshot(
                        &before
                            .iter()
                            .map(|(path, _, _)| path.clone())
                            .collect::<Vec<_>>()
                    )?,
                    before
                );
                bail!("stop the repair before writing anything")
            });
        assert!(error.is_err());
        assert!(
            entered,
            "data-only admission must not disable the repair writer"
        );
        assert_eq!(self.manifest, input);
        assert_eq!(snapshot(&self.watched(data))?, before);
        Ok(())
    }
}

// Preserve directories and missing entries as well as file bytes. Recovery may
// create its stable lock files, which are deliberately outside this inventory.
fn snapshot(paths: &[PathBuf]) -> Result<Vec<(PathBuf, bool, Option<Vec<u8>>)>> {
    paths
        .iter()
        .map(|path| {
            let kind = match fs::symlink_metadata(path) {
                Ok(metadata) => Some(metadata),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            Ok((
                path.clone(),
                kind.as_ref().is_some_and(|metadata| metadata.is_dir()),
                if kind.as_ref().is_some_and(|metadata| metadata.is_file()) {
                    Some(fs::read(path)?)
                } else {
                    None
                },
            ))
        })
        .collect()
}

#[test]
fn missing_published_data_keeps_fallbacks_until_explicit_restoration() -> Result<()> {
    for tier in [TierKind::Fast, TierKind::Quality] {
        let temp = tempfile::tempdir()?;
        let data = temp.path();
        let mut fixture = Fixture::new(data, tier)?;
        let approval = plan_backfill_artifacts(data)?;
        let reader = VectorIndex::open_read_only(&fixture.live)?;
        let query = HashEmbedder::default().embed_sync("compiler publication recovery")?;
        let hits = reader.search_top_k(&query, 3, None)?;
        let original = fs::read(&fixture.live)?;
        let retained = data.join("retained-publication.fsvi");
        fs::rename(&fixture.live, &retained)?;
        fixture.assert_retained(data, &approval)?;
        assert_eq!(fs::read(&retained)?, original);
        assert_eq!(reader.search_top_k(&query, 3, None)?, hits);
        fs::rename(&retained, &fixture.live)?;
        let refreshed = plan_backfill_artifacts(data)?;
        let report = apply_backfill_artifact_plan(data, &refreshed.plan_fingerprint)?;
        assert_eq!((report.removed_files, report.removed_directories), (1, 1));
        assert!(report.failed_paths.is_empty());
        assert_eq!(fs::read(&fixture.live)?, original);
        assert_eq!(reader.search_top_k(&query, 3, None)?, hits);
        assert_eq!(
            VectorIndex::open_read_only(&fixture.live)?.search_top_k(&query, 3, None)?,
            hits
        );
    }
    Ok(())
}

#[test]
fn damaged_or_foreign_publications_never_authorize_retiring_complete_copies() -> Result<()> {
    for damage in [
        "empty",
        "truncated",
        "producer",
        "dimension",
        "coverage",
        "wal-directory",
    ] {
        let temp = tempfile::tempdir()?;
        let data = temp.path();
        let mut fixture = Fixture::new(data, TierKind::Quality)?;
        let approval = plan_backfill_artifacts(data)?;
        match damage {
            "empty" => fs::write(&fixture.live, [])?,
            "truncated" => {
                let original = fs::read(&fixture.live)?;
                fs::write(&fixture.live, &original[..original.len() / 2])?;
            }
            "wal-directory" => fs::create_dir(wal_path_for(&fixture.live))?,
            "coverage" => {
                let mut index = VectorIndex::open(&fixture.live)?;
                let id = index.doc_id_at(0)?.to_owned();
                assert!(index.soft_delete(&id)?);
            }
            _ => {
                let dimension = if damage == "dimension" { 4 } else { 384 };
                let producer = if damage == "producer" {
                    "foreign"
                } else {
                    "fnv1a-384"
                };
                let mut writer = VectorIndex::create_with_revision(
                    &fixture.live,
                    producer,
                    "fixture",
                    dimension,
                    Quantization::F16,
                )?;
                let mut vector = vec![0.0; dimension];
                vector[0] = 1.0;
                for id in ["one", "two", "three"] {
                    writer.write_record(id, &vector)?;
                }
                writer.finish()?;
            }
        }
        fixture.assert_retained(data, &approval)?;
    }
    Ok(())
}

#[test]
fn acknowledged_wal_rows_must_survive_before_published_fallbacks_are_reclaimed() -> Result<()> {
    for missing in [false, true] {
        let temp = tempfile::tempdir()?;
        let data = temp.path();
        let mut fixture = Fixture::new(data, TierKind::Quality)?;
        let mut writer = VectorIndex::open(&fixture.live)?;
        let vector = writer.vector_at_f32(0)?;
        writer.append("wal-only-fourth-document", &vector)?;
        drop(writer);
        fixture.manifest.quality_tier.as_mut().unwrap().doc_count = 4;
        fixture.manifest.save(data)?;
        let approval = plan_backfill_artifacts(data)?;
        let wal = wal_path_for(&fixture.live);
        let original = fs::read(&wal)?;
        if missing {
            fs::rename(&wal, data.join("retained-publication.wal"))?;
        } else {
            fs::write(&wal, &original[..original.len() - 1])?;
        }
        fixture.assert_retained(data, &approval)?;
        fs::write(&wal, &original)?;
        let before = snapshot(&[
            fixture.live.clone(),
            wal.clone(),
            SemanticManifest::path(data),
        ])?;
        let current = plan_backfill_artifacts(data)?;
        let report = apply_backfill_artifact_plan(data, &current.plan_fingerprint)?;
        assert_eq!((report.removed_files, report.removed_directories), (1, 1));
        assert_eq!(
            snapshot(
                &before
                    .iter()
                    .map(|(path, _, _)| path.clone())
                    .collect::<Vec<_>>()
            )?,
            before
        );
    }
    Ok(())
}

#[test]
fn repair_of_a_missing_publication_retains_fallbacks_until_new_publication_succeeds() -> Result<()>
{
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let mut fixture = Fixture::new(data, TierKind::Quality)?;
    let approval = plan_backfill_artifacts(data)?;
    fs::rename(&fixture.live, data.join("retained-publication.fsvi"))?;
    fixture.assert_retained(data, &approval)?;
    let published = fixture.indexer.run_backfill_batch(
        &rows(),
        data,
        &mut fixture.manifest,
        plan(TierKind::Quality, "content-v1:repaired"),
    )?;
    assert!(published.published);
    assert!(fixture.manifest.checkpoint.is_none());
    assert!(!fixture.staging.exists() && !fixture.reuse.exists());
    assert_eq!(
        VectorIndex::open_read_only(&published.index_path)?.record_count(),
        3
    );
    assert!(data.join("retained-publication.fsvi").is_file());
    Ok(())
}

#[test]
fn optional_unready_records_stay_protected_without_becoming_cleanup_authority() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let mut fixture = Fixture::new(data, TierKind::Quality)?;
    let mut optional = fixture.manifest.quality_tier.clone().unwrap();
    optional.tier = TierKind::Fast;
    optional.ready = false;
    optional.index_path = fixture
        .reuse
        .join("candidate.fsvi")
        .strip_prefix(data)?
        .to_string_lossy()
        .into_owned();
    fs::write(
        data.join(&optional.index_path),
        b"unready recovery evidence",
    )?;
    fixture.manifest.fast_tier = Some(optional);
    fixture.manifest.save(data)?;
    let report = reclaim_backfill_artifacts(data)?;
    assert_eq!((report.removed_files, report.removed_directories), (1, 0));
    assert_eq!(
        fs::read(fixture.reuse.join("candidate.fsvi"))?,
        b"unready recovery evidence"
    );
    assert_eq!(
        VectorIndex::open_read_only(&fixture.live)?.record_count(),
        3
    );
    Ok(())
}

#[test]
fn data_pin_failures_retain_their_type_and_no_pin_runs_on_an_invalid_image() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let fixture = Fixture::new(data, TierKind::Quality)?;
    let before = snapshot(&fixture.watched(data))?;
    let mut pins = 0;
    let error = validate_with(data, &fixture.manifest, |path, main, _, wal| {
        pins += 1;
        assert_eq!(path, fixture.live);
        assert_eq!(main.metadata()?.len(), fs::metadata(path)?.len());
        assert!(wal.is_none());
        Err(io::Error::other("injected publication fsync failure").into())
    })
    .unwrap_err();
    assert!(error.is::<PublicationUnavailable>() && error.is::<io::Error>());
    assert_eq!(pins, 1);
    assert_eq!(snapshot(&fixture.watched(data))?, before);
    fs::write(&fixture.live, [])?;
    assert!(
        validate_with(data, &fixture.manifest, |_, _, _, _| {
            pins += 1;
            Ok(())
        })
        .is_err()
    );
    assert_eq!(pins, 1);
    Ok(())
}

#[test]
fn invalid_authority_still_refuses_startup_and_empty_sweeps_skip_data_admission() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let mut fixture = Fixture::new(data, TierKind::Quality)?;
    fs::rename(&fixture.live, data.join("retained-publication.fsvi"))?;
    fs::write(SemanticManifest::path(data), b"{torn authority")?;
    let before = snapshot(&fixture.watched(data))?;
    let mut entered = false;
    assert!(
        fixture
            .indexer
            .with_backfill_artifacts(data, &mut fixture.manifest, |_, _| {
                entered = true;
                bail!("must not enter through invalid authority")
            })
            .is_err()
    );
    assert!(!entered);
    assert_eq!(snapshot(&fixture.watched(data))?, before);
    fixture.manifest.save(data)?;
    // Park all candidate paths rather than deleting test evidence. With no
    // candidate removal, an unusable publication need not be opened or pinned.
    fs::rename(&fixture.staging, data.join("retained-staging.fsvi"))?;
    fs::rename(&fixture.reuse, data.join("retained-reuse"))?;
    let result = reclaim_backfill_artifacts(data)?;
    assert_eq!(result.removed_files + result.removed_directories, 0);
    Ok(())
}
