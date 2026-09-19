//! Repeated publication polling must not reacquire already-sealed artifacts.
//! Removing access is the I/O oracle; no timing threshold can hide a reread.
use super::*;

fn hide_artifacts(root: &Path, manifest: &SemanticGenerationManifestV1) -> TestResult {
    for artifact in &manifest.artifacts {
        let path = manifest.generation_dir(root)?.join(&artifact.relative_path);
        fs::rename(&path, path.with_extension("retained"))?;
    }
    Ok(())
}

#[test]
fn unchanged_sharded_refresh_reuses_every_vector_and_native_graph() -> TestResult {
    let root = tempfile::tempdir()?;
    let mut manifest = partitioned(root.path(), "poll-retained", 1, Some(QUALITY));
    attach_graphs(root.path(), &mut manifest, 1);
    let pointer = select(root.path(), &manifest, None);
    let mut reader = open(root.path(), &manifest).with_ann(AnnAdmissionBudget::default())?;
    let identity = Arc::clone(&reader.identity);
    let ann = Arc::clone(&reader.reader.ann);
    hide_artifacts(root.path(), &manifest)?;
    let before = snapshot(root.path());
    for _ in 0..32 {
        let changed =
            reader.refresh_current(&manifest.corpus, SemanticSelectionBudget::default())?;
        assert!(!changed);
    }
    assert!(Arc::ptr_eq(&identity, &reader.identity));
    assert!(Arc::ptr_eq(&ann, &reader.reader.ann));
    for tier in [TierKind::Fast, TierKind::Quality] {
        assert_eq!(reader.shard_count(tier), 3);
    }
    let fast = native_batch(&reader);
    assert_eq!(first_id(&fast), 3);
    assert_engine(&fast, SemanticShardEngine::NativeAnn);
    let quality = TieredQueryEmbeddings::quality_only(query(SemanticArtifactRole::QualityVector));
    let quality = reader
        .activate(&quality)?
        .search_with_ann(1, None, native_policy())?;
    assert_eq!(first_id(&quality), 1);
    assert_engine(&quality, SemanticShardEngine::NativeAnn);
    assert_eq!(reader.selection().pointer(), &pointer);
    assert_eq!(snapshot(root.path()), before);
    assert!(
        SelectedSemanticGeneration::open_current(
            root.path(),
            &manifest.corpus,
            SemanticSelectionBudget::default()
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn stale_refresh_is_rejected_before_opening_its_missing_shards() -> TestResult {
    let root = tempfile::tempdir()?;
    let old = partitioned(root.path(), "poll-old", 1, None);
    let current = partitioned(root.path(), "poll-current", 2, None);
    let first = select(root.path(), &old, None);
    let second = select(root.path(), &current, Some(&first));
    let mut reader = open(root.path(), &current);
    hide_artifacts(root.path(), &old)?;
    for pointer in [
        first,
        SemanticCurrentPointerV1::for_manifest(&old, 99, second.selection_epoch)?,
    ] {
        fs::write(
            SemanticCurrentPointerV1::path(root.path()),
            pointer.canonical_bytes()?,
        )?;
        let result = reader.refresh_current(&old.corpus, SemanticSelectionBudget::default());
        assert!(
            matches!(result, Err(SemanticSelectionError::StaleSelection)),
            "stale metadata must not trigger artifact admission: {result:?}"
        );
        assert_eq!(reader.selection().pointer(), &second);
        assert_eq!(first_id(&reader.activate(&queries())?.search(1, None)?), 3);
    }
    Ok(())
}

#[test]
fn unchanged_refresh_still_enforces_new_budget_and_expected_corpus() -> TestResult {
    let root = tempfile::tempdir()?;
    let manifest = partitioned(root.path(), "poll-budget", 1, Some(QUALITY));
    let pointer = select(root.path(), &manifest, None);
    let mut reader = open(root.path(), &manifest);
    let total = manifest
        .artifacts
        .iter()
        .map(|artifact| artifact.size_bytes)
        .sum::<u64>();
    hide_artifacts(root.path(), &manifest)?;
    assert!(matches!(
        reader.refresh_current(
            &manifest.corpus,
            SemanticSelectionBudget {
                max_declared_vector_bytes: total - 1
            }
        ),
        Err(SemanticSelectionError::BudgetExceeded)
    ));
    let mut foreign = manifest.corpus.clone();
    foreign.source_change_sequence += 1;
    assert!(matches!(
        reader.refresh_current(&foreign, SemanticSelectionBudget::default()),
        Err(SemanticSelectionError::Publication(
            SemanticGenerationError::StaleCorpus { .. }
        ))
    ));
    assert!(!reader.refresh_current(
        &manifest.corpus,
        SemanticSelectionBudget {
            max_declared_vector_bytes: total
        }
    )?);
    assert_eq!(reader.selection().pointer(), &pointer);
    Ok(())
}

#[test]
fn unchanged_refresh_does_not_hide_manifest_or_pointer_tampering() -> TestResult {
    let root = tempfile::tempdir()?;
    let manifest = partitioned(root.path(), "poll-authenticated", 1, None);
    let pointer = select(root.path(), &manifest, None);
    let mut reader = open(root.path(), &manifest);
    let original = fs::read(manifest.manifest_path(root.path())?)?;
    let mut changed = manifest.clone();
    changed.build_id.push_str("-tampered");
    fs::write(
        manifest.manifest_path(root.path())?,
        serde_json::to_vec(&changed)?,
    )?;
    assert!(matches!(
        reader.refresh_current(&manifest.corpus, SemanticSelectionBudget::default()),
        Err(SemanticSelectionError::Publication(
            SemanticGenerationError::ManifestDigestMismatch { .. }
        ))
    ));
    fs::write(manifest.manifest_path(root.path())?, original)?;
    let path = SemanticCurrentPointerV1::path(root.path());
    fs::rename(&path, path.with_extension("retained"))?;
    assert!(matches!(
        reader.refresh_current(&manifest.corpus, SemanticSelectionBudget::default()),
        Err(SemanticSelectionError::Publication(
            SemanticGenerationError::MissingPointer
        ))
    ));
    assert_eq!(reader.selection().pointer(), &pointer);
    assert_eq!(first_id(&reader.activate(&queries())?.search(1, None)?), 3);
    Ok(())
}

#[test]
fn changed_selection_never_reuses_same_named_corrupt_successor_artifacts() -> TestResult {
    let root = tempfile::tempdir()?;
    let old = partitioned(root.path(), "poll-before", 1, Some(QUALITY));
    let new = partitioned(root.path(), "poll-after", 2, Some(QUALITY));
    let first = select(root.path(), &old, None);
    let mut reader = open(root.path(), &old);
    let identity = Arc::clone(&reader.identity);
    let second = select(root.path(), &new, Some(&first));
    let path = new
        .generation_dir(root.path())?
        .join(&new.artifacts[2].relative_path);
    let mut bytes = fs::read(&path)?;
    *bytes.last_mut().unwrap() ^= 1;
    fs::write(&path, bytes)?;
    assert!(
        reader
            .refresh_current(&new.corpus, SemanticSelectionBudget::default())
            .is_err()
    );
    assert!(Arc::ptr_eq(&identity, &reader.identity));
    assert_eq!(reader.selection().pointer(), &first);
    assert_ne!(reader.selection().pointer(), &second);
    assert_eq!(first_id(&reader.activate(&queries())?.search(1, None)?), 3);
    Ok(())
}

#[test]
fn unchanged_refresh_preserves_ann_opt_out_and_retained_clones() -> TestResult {
    let root = tempfile::tempdir()?;
    let mut manifest = partitioned(root.path(), "poll-opt-out", 1, None);
    attach_graphs(root.path(), &mut manifest, 1);
    select(root.path(), &manifest, None);
    let native = open(root.path(), &manifest).with_ann(AnnAdmissionBudget::default())?;
    let mut exact = native.clone().without_ann();
    hide_artifacts(root.path(), &manifest)?;
    assert!(!exact.refresh_current(&manifest.corpus, SemanticSelectionBudget::default())?);
    assert!(exact.ann_budget.is_none());
    // An ANN request against an opted-out reader must report the existing
    // exact fallback contract, rather than pretend a graph was used.
    let batch = native_batch(&exact);
    assert_engine(&batch, SemanticShardEngine::ExactFallback);
    for report in batch.batch().execution() {
        assert_eq!(report.fallback_reason, Some(AnnFallbackReason::NotSelected));
        assert_eq!(report.ann_windows, 0);
        assert_eq!(report.candidate_rows, 0);
        assert!(report.graph_sha256.is_none());
    }
    assert_eq!(first_id(&batch), 3);
    // No ANN request has the distinct plain-exact reporting contract.
    let ordinary = exact.activate(&queries())?.search(1, None)?;
    assert_engine(&ordinary, SemanticShardEngine::Exact);
    assert_eq!(first_id(&ordinary), 3);
    assert_engine(&native_batch(&native), SemanticShardEngine::NativeAnn);
    Ok(())
}

#[test]
fn legacy_singleton_refresh_also_reuses_its_sealed_owner() -> TestResult {
    let root = tempfile::tempdir()?;
    let manifest = fixture(root.path(), "poll-v1", 1, Some(A), None);
    select(root.path(), &manifest, None);
    let mut reader = open(root.path(), &manifest);
    hide_artifacts(root.path(), &manifest)?;
    assert!(!reader.refresh_current(&manifest.corpus, SemanticSelectionBudget::default())?);
    assert_eq!(reader.shard_count(TierKind::Fast), 1);
    assert_eq!(first_id(&reader.activate(&queries())?.search(1, None)?), 1);
    Ok(())
}
