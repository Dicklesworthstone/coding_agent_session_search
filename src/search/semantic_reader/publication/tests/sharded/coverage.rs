//! Canonical-corpus membership and publication replacement across shard layouts.
use super::*;

fn orient(
    mut manifest: SemanticGenerationManifestV1,
    reverse: bool,
) -> SemanticGenerationManifestV1 {
    if reverse {
        for artifact in &mut manifest.artifacts {
            artifact.role = match artifact.role {
                SemanticArtifactRole::FastVector => SemanticArtifactRole::QualityVector,
                SemanticArtifactRole::QualityVector => SemanticArtifactRole::FastVector,
                _ => panic!("fixture must contain only vectors"),
            };
        }
        manifest
            .artifacts
            .sort_by_key(|artifact| (artifact.role, artifact.shard.as_ref().unwrap().ordinal));
    }
    manifest
}

fn reject_foreign_partial(reverse: bool) -> TestResult {
    let root = tempfile::tempdir()?;
    let foreign = [QUALITY[0], (4, [0.0, 1.0, 0.0, 0.0], true)];
    let manifest = orient(
        partitioned(root.path(), "foreign-partial", 1, Some(&foreign)),
        reverse,
    );
    manifest.validate()?;
    select(root.path(), &manifest, None);
    let before = snapshot(root.path());
    let result = SelectedSemanticGeneration::open_current(
        root.path(),
        &manifest.corpus,
        SemanticSelectionBudget::default(),
    );
    match result {
        Err(SemanticSelectionError::ArtifactMismatch {
            field: "partial_live_docset",
            ..
        }) => {}
        Ok(_) => panic!("partial tier admitted a document outside the authenticated full corpus"),
        Err(error) => panic!("unexpected admission failure before membership check: {error}"),
    }
    assert_eq!(before, snapshot(root.path()));
    Ok(())
}

#[test]
fn partial_quality_must_belong_to_the_complete_fast_corpus() -> TestResult {
    reject_foreign_partial(false)
}

#[test]
fn partial_fast_must_belong_to_the_complete_quality_corpus() -> TestResult {
    reject_foreign_partial(true)
}

#[test]
fn sparse_partial_tier_is_a_valid_subset_in_either_direction() -> TestResult {
    for reverse in [false, true] {
        let root = tempfile::tempdir()?;
        let sparse = [QUALITY[0], QUALITY[2]];
        let manifest = orient(
            partitioned(root.path(), "sparse-valid", 1, Some(&sparse)),
            reverse,
        );
        select(root.path(), &manifest, None);
        let reader = SelectedSemanticGeneration::open_current(
            root.path(),
            &manifest.corpus,
            SemanticSelectionBudget::default(),
        )?;
        let tier = if reverse {
            TierKind::Fast
        } else {
            TierKind::Quality
        };
        assert_eq!(reader.shard_count(tier), 2);
        assert_eq!(
            reader.witness_at(tier, 0).unwrap().live_count
                + reader.witness_at(tier, 1).unwrap().live_count,
            2
        );
    }
    Ok(())
}

#[test]
fn refresh_and_explicit_rollback_cross_v1_v2_partitions_without_revoking_leases() -> TestResult {
    let root = tempfile::tempdir()?;
    let old = fixture(root.path(), "single-old", 1, Some(A), None);
    let first = select(root.path(), &old, None);
    let mut reader = open(root.path(), &old);
    let q = queries();
    let old_batch = reader.activate(&q)?.search(1, None)?;
    let multi = partitioned(root.path(), "multi-next", 2, None);
    let second = select(root.path(), &multi, Some(&first));
    assert!(reader.refresh_current(&multi.corpus, SemanticSelectionBudget::default())?);
    assert_eq!(reader.shard_count(TierKind::Fast), 3);
    assert_eq!(first_id(&reader.activate(&q)?.search(1, None)?), 3);
    let retained_multi = reader.clone();
    let rollback = select(root.path(), &old, Some(&second));
    assert!(reader.refresh_current(&old.corpus, SemanticSelectionBudget::default())?);
    assert_eq!(reader.shard_count(TierKind::Fast), 1);
    assert_eq!(reader.selection().pointer(), &rollback);
    assert_eq!(first_id(&reader.activate(&q)?.search(1, None)?), 1);
    assert_eq!(old_batch.selection().pointer(), &first);
    let path = multi
        .generation_dir(root.path())?
        .join(&multi.artifacts[2].relative_path);
    fs::rename(&path, path.with_extension("retired-owner"))?;
    let retained = retained_multi.activate(&q)?.search(1, None)?;
    assert_eq!(first_id(&retained), 3);
    assert_eq!(retained.selection().pointer(), &second);
    Ok(())
}

#[test]
fn missing_successor_shard_cannot_replace_a_working_selected_reader() -> TestResult {
    let root = tempfile::tempdir()?;
    let old = fixture(root.path(), "valid-old", 1, Some(A), None);
    let first = select(root.path(), &old, None);
    let mut reader = open(root.path(), &old);
    let multi = partitioned(root.path(), "incomplete-next", 2, None);
    select(root.path(), &multi, Some(&first));
    let path = multi
        .generation_dir(root.path())?
        .join(&multi.artifacts[2].relative_path);
    fs::rename(&path, path.with_extension("not-selected"))?;
    assert!(
        reader
            .refresh_current(&multi.corpus, SemanticSelectionBudget::default())
            .is_err()
    );
    assert_eq!(reader.selection().pointer(), &first);
    assert_eq!(reader.shard_count(TierKind::Fast), 1);
    assert_eq!(first_id(&reader.activate(&queries())?.search(1, None)?), 1);
    Ok(())
}

#[test]
fn sharded_publication_restarts_in_a_fresh_process_with_both_tiers() -> TestResult {
    use std::process::{Command, Stdio};
    use wait_timeout::ChildExt;
    let root = tempfile::tempdir()?;
    let manifest = partitioned(root.path(), "fresh-multi", 1, Some(QUALITY));
    let pointer = select(root.path(), &manifest, None);
    let mut child = Command::new(std::env::current_exe()?)
        .args(["--exact", "search::semantic_reader::publication::tests::sharded::coverage::fresh_process_multi_shard_child", "--nocapture", "--test-threads=1"])
        .env("CASS_TEST_SHARD_RESTART_ROOT", root.path())
        .env("CASS_TEST_SHARD_EXPECTED_CORPUS", serde_json::to_string(&manifest.corpus)?)
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
    let status = match child.wait_timeout(std::time::Duration::from_secs(10))? {
        Some(status) => status,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("fresh-process publication reader exceeded its test budget");
        }
    };
    assert!(status.success(), "fresh-process reader failed");
    // A distinct on-disk receipt proves the exact child test ran; exit zero
    // from an empty test filter would not create it.
    let receipt: serde_json::Value =
        serde_json::from_slice(&fs::read(root.path().join("restart-receipt.json"))?)?;
    assert_eq!(receipt["pointer"], serde_json::to_value(pointer)?);
    assert_eq!(receipt["shards"], serde_json::json!([3, 3]));
    assert_eq!(receipt["fast_hit"], 3);
    assert_eq!(receipt["quality_hit"], 1);
    Ok(())
}

#[test]
fn fresh_process_multi_shard_child() -> TestResult {
    let Ok(root) = dotenvy::var("CASS_TEST_SHARD_RESTART_ROOT") else {
        return Ok(());
    };
    let root = std::path::PathBuf::from(root);
    let expected: SemanticCorpusSnapshotIdentity =
        serde_json::from_str(&dotenvy::var("CASS_TEST_SHARD_EXPECTED_CORPUS")?)?;
    let reader = SelectedSemanticGeneration::open_current(
        &root,
        &expected,
        SemanticSelectionBudget::default(),
    )?;
    let fast_query = queries();
    let fast = reader.activate(&fast_query)?.search(1, None)?;
    let quality_query =
        TieredQueryEmbeddings::quality_only(query(SemanticArtifactRole::QualityVector));
    let quality = reader.activate(&quality_query)?.search(1, None)?;
    let receipt = serde_json::json!({
        "pointer": reader.selection().pointer(),
        "shards": [reader.shard_count(TierKind::Fast), reader.shard_count(TierKind::Quality)],
        "fast_hit": first_id(&fast),
        "quality_hit": first_id(&quality),
    });
    fs::write(
        root.join("restart-receipt.json"),
        serde_json::to_vec(&receipt)?,
    )?;
    Ok(())
}
