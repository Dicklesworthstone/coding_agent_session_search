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

#[test]
fn fresh_open_survives_a_missing_graph_without_hiding_it_from_disk_audit() -> TestResult {
    let root = tempfile::tempdir()?;
    let mut manifest = partitioned(root.path(), "missing-optional-graph", 1, Some(QUALITY));
    attach_graphs(root.path(), &mut manifest, 1);
    let pointer = select(root.path(), &manifest, None);
    let graph = manifest.generation_dir(root.path())?.join(
        &manifest
            .artifact_at(SemanticArtifactRole::FastAnn, 2)
            .unwrap()
            .relative_path,
    );
    fs::rename(&graph, graph.with_extension("retained"))?;
    let before = snapshot(root.path());
    assert!(load_current_semantic_generation(root.path(), Some(&manifest.corpus)).is_err());
    let reader = open(root.path(), &manifest);
    let exact = reader.activate(&queries())?.search(3, None)?;
    assert_engine(&exact, SemanticShardEngine::Exact);
    assert_eq!(first_id(&exact), 3);
    assert_eq!(reader.selection().pointer(), &pointer);
    assert_eq!(reader.selection().manifest(), &manifest);

    let reader = reader.with_ann(AnnAdmissionBudget::default())?;
    let batch = reader
        .activate(&queries())?
        .search_with_ann(3, None, native_policy())?;
    assert_eq!(
        batch
            .batch()
            .hits()
            .iter()
            .map(|hit| hit.document.message_id)
            .collect::<Vec<_>>(),
        exact
            .batch()
            .hits()
            .iter()
            .map(|hit| hit.document.message_id)
            .collect::<Vec<_>>()
    );
    let execution = batch.batch().execution();
    assert_eq!(execution.len(), 3);
    assert_eq!(execution[0].engine, SemanticShardEngine::NativeAnn);
    assert_eq!(execution[1].engine, SemanticShardEngine::NativeAnn);
    assert_eq!(execution[2].engine, SemanticShardEngine::ExactFallback);
    assert_eq!(
        execution[2].fallback_reason,
        Some(AnnFallbackReason::SidecarUnavailable)
    );
    assert_eq!(execution[2].ann_windows, 0);
    for ordinal in 0..3 {
        assert!(matches!(
            reader.ann_admission_at(TierKind::Quality, ordinal),
            Some(SemanticAnnAdmission::Admitted { .. })
        ));
    }
    assert_eq!(before, snapshot(root.path()));
    Ok(())
}

#[test]
fn corrupt_optional_graphs_keep_exact_results_but_cannot_be_resealed() -> TestResult {
    for change_length in [false, true] {
        let root = tempfile::tempdir()?;
        let mut manifest = partitioned(root.path(), "corrupt-optional-graph", 1, None);
        attach_graphs(root.path(), &mut manifest, 1);
        select(root.path(), &manifest, None);
        let graph = manifest.generation_dir(root.path())?.join(
            &manifest
                .artifact_at(SemanticArtifactRole::FastAnn, 2)
                .unwrap()
                .relative_path,
        );
        let mut bytes = fs::read(&graph)?;
        if change_length {
            bytes.push(0);
        } else {
            *bytes.last_mut().unwrap() ^= 1;
        }
        fs::write(&graph, bytes)?;
        let before = snapshot(root.path());
        assert!(load_current_semantic_generation(root.path(), Some(&manifest.corpus)).is_err());
        assert!(manifest.write_immutable(root.path()).is_err());
        let reader = open(root.path(), &manifest).with_ann(AnnAdmissionBudget::default())?;
        let batch = reader
            .activate(&queries())?
            .search_with_ann(3, None, native_policy())?;
        assert_eq!(first_id(&batch), 3);
        let execution = batch.batch().execution();
        assert_eq!(execution[0].engine, SemanticShardEngine::NativeAnn);
        assert_eq!(execution[1].engine, SemanticShardEngine::NativeAnn);
        assert_eq!(execution[2].engine, SemanticShardEngine::ExactFallback);
        assert_eq!(execution[2].ann_windows, 0);
        assert!(execution[2].graph_sha256.is_none());
        assert_eq!(before, snapshot(root.path()));
    }
    Ok(())
}

#[test]
fn zero_graph_budget_preserves_exact_open_but_never_waives_a_missing_vector() -> TestResult {
    let root = tempfile::tempdir()?;
    let mut manifest = partitioned(root.path(), "optional-budget-zero", 1, None);
    attach_graphs(root.path(), &mut manifest, 1);
    select(root.path(), &manifest, None);
    let directory = manifest.generation_dir(root.path())?;
    for artifact in manifest.artifacts_for(SemanticArtifactRole::FastAnn) {
        let graph = directory.join(&artifact.relative_path);
        fs::rename(&graph, graph.with_extension("retained"))?;
    }
    let before = snapshot(root.path());
    let reader = open(root.path(), &manifest).with_ann(AnnAdmissionBudget {
        max_declared_graph_bytes: 0,
    })?;
    let batch = native_batch(&reader);
    assert_eq!(first_id(&batch), 3);
    assert_engine(&batch, SemanticShardEngine::ExactFallback);
    for report in batch.batch().execution() {
        assert_eq!(
            report.fallback_reason,
            Some(AnnFallbackReason::AdmissionBudget)
        );
        assert_eq!(report.ann_windows, 0);
    }
    assert_eq!(before, snapshot(root.path()));
    let vector = directory.join(
        &manifest
            .artifact_at(SemanticArtifactRole::FastVector, 2)
            .unwrap()
            .relative_path,
    );
    fs::rename(&vector, vector.with_extension("retained"))?;
    assert!(
        SelectedSemanticGeneration::open_current(
            root.path(),
            &manifest.corpus,
            SemanticSelectionBudget::default(),
        )
        .is_err()
    );
    // The already-sealed owner is still searchable; a new reader cannot open.
    assert_eq!(first_id(&native_batch(&reader)), 3);
    Ok(())
}

#[cfg(unix)]
#[test]
fn optional_graph_admission_rejects_linked_parent_directories() -> TestResult {
    let root = tempfile::tempdir()?;
    let mut manifest = partitioned(root.path(), "linked-optional-graph", 1, None);
    attach_graphs(root.path(), &mut manifest, 1);
    let directory = manifest.generation_dir(root.path())?;
    let artifact = manifest
        .artifacts
        .iter_mut()
        .find(|artifact| {
            artifact.role == SemanticArtifactRole::FastAnn
                && artifact
                    .shard
                    .as_ref()
                    .is_some_and(|shard| shard.ordinal == 2)
        })
        .unwrap();
    let original = directory.join(&artifact.relative_path);
    let graph_dir = directory.join("optional-graphs");
    fs::create_dir(&graph_dir)?;
    let relocated = graph_dir.join(original.file_name().unwrap());
    fs::rename(&original, &relocated)?;
    fs::rename(
        native_hnsw_generation_receipt_path(&original)?,
        native_hnsw_generation_receipt_path(&relocated)?,
    )?;
    artifact.relative_path = format!(
        "optional-graphs/{}",
        relocated.file_name().unwrap().to_str().unwrap()
    );
    manifest.validate()?;
    select(root.path(), &manifest, None);
    let external = root.path().join("outside-generation");
    fs::rename(&graph_dir, &external)?;
    std::os::unix::fs::symlink(&external, &graph_dir)?;
    let before = snapshot(root.path());
    assert!(load_current_semantic_generation(root.path(), Some(&manifest.corpus)).is_err());
    let reader = open(root.path(), &manifest).with_ann(AnnAdmissionBudget::default())?;
    let batch = reader
        .activate(&queries())?
        .search_with_ann(3, None, native_policy())?;
    assert_eq!(first_id(&batch), 3);
    let execution = batch.batch().execution();
    assert_eq!(execution[0].engine, SemanticShardEngine::NativeAnn);
    assert_eq!(execution[1].engine, SemanticShardEngine::NativeAnn);
    assert_eq!(execution[2].engine, SemanticShardEngine::ExactFallback);
    assert_eq!(
        execution[2].fallback_reason,
        Some(AnnFallbackReason::UnsafePath)
    );
    assert_eq!(execution[2].ann_windows, 0);
    assert_eq!(before, snapshot(root.path()));
    assert!(fs::symlink_metadata(&graph_dir)?.file_type().is_symlink());
    Ok(())
}

#[test]
fn refresh_adopts_valid_successor_vectors_when_one_optional_graph_is_missing() -> TestResult {
    let root = tempfile::tempdir()?;
    let mut old = partitioned(root.path(), "optional-before", 1, Some(QUALITY));
    let mut new = partitioned(root.path(), "optional-after", 2, Some(QUALITY));
    attach_graphs(root.path(), &mut old, 1);
    attach_graphs(root.path(), &mut new, 2);
    let first = select(root.path(), &old, None);
    let mut reader = open(root.path(), &old).with_ann(AnnAdmissionBudget::default())?;
    let retained = reader.clone();
    let old_batch = native_batch(&retained);
    let second = select(root.path(), &new, Some(&first));
    let graph = new.generation_dir(root.path())?.join(
        &new.artifact_at(SemanticArtifactRole::FastAnn, 2)
            .unwrap()
            .relative_path,
    );
    fs::rename(&graph, graph.with_extension("retained"))?;
    let before = snapshot(root.path());
    assert!(reader.refresh_current(&new.corpus, SemanticSelectionBudget::default())?);
    assert_eq!(reader.selection().pointer(), &second);
    let batch = reader
        .activate(&queries())?
        .search_with_ann(3, None, native_policy())?;
    assert_eq!(first_id(&batch), 3);
    assert_eq!(batch.selection().pointer(), &second);
    assert_eq!(
        batch.batch().execution()[2].engine,
        SemanticShardEngine::ExactFallback
    );
    assert_eq!(old_batch.selection().pointer(), &first);
    assert_eq!(retained.selection().pointer(), &first);
    assert_engine(&native_batch(&retained), SemanticShardEngine::NativeAnn);
    assert!(!reader.refresh_current(&new.corpus, SemanticSelectionBudget::default())?);
    assert_eq!(before, snapshot(root.path()));
    Ok(())
}

#[test]
fn unretained_quality_graphs_do_not_spend_the_fast_only_graph_budget() -> TestResult {
    let root = tempfile::tempdir()?;
    let mut manifest = partitioned(root.path(), "selected-ann-budget", 1, Some(QUALITY));
    attach_graphs(root.path(), &mut manifest, 1);
    select(root.path(), &manifest, None);
    let mut reader = open(root.path(), &manifest).reader;
    reader.quality = None;
    let fast_bytes = manifest
        .artifacts_for(SemanticArtifactRole::FastAnn)
        .map(|artifact| artifact.size_bytes)
        .sum::<u64>();
    let directory = manifest.generation_dir(root.path())?;
    // The retained fast selection must not try to open the unretained tier.
    for artifact in manifest.artifacts_for(SemanticArtifactRole::QualityAnn) {
        let graph = directory.join(&artifact.relative_path);
        fs::rename(&graph, graph.with_extension("retained"))?;
    }
    let before = snapshot(root.path());
    let reader = reader.with_manifest_ann(
        &manifest,
        &directory,
        AnnAdmissionBudget {
            max_declared_graph_bytes: fast_bytes,
        },
    );
    for ordinal in 0..3 {
        assert!(matches!(
            reader.ann_admission(TierKind::Fast, ordinal),
            Some(SemanticAnnAdmission::Admitted { .. })
        ));
        assert!(reader.ann_admission(TierKind::Quality, ordinal).is_none());
    }
    let batch = reader
        .activate(&queries())?
        .search_with_ann(3, None, native_policy())?;
    assert_eq!(batch.hits()[0].document.message_id, 3);
    assert!(
        batch
            .execution()
            .iter()
            .all(|report| report.engine == SemanticShardEngine::NativeAnn)
    );
    assert_eq!(before, snapshot(root.path()));
    Ok(())
}

#[test]
fn explicit_ann_budget_is_aggregate_and_checked_before_missing_sidecars() -> TestResult {
    use crate::search::semantic_reader::ann::SemanticAnnExpectation;

    let root = tempfile::tempdir()?;
    let mut manifest = partitioned(root.path(), "explicit-ann-budget", 1, Some(QUALITY));
    attach_graphs(root.path(), &mut manifest, 1);
    select(root.path(), &manifest, None);
    let reader = open(root.path(), &manifest).reader;
    let directory = manifest.generation_dir(root.path())?;
    let expectations = |tier, role| {
        reader
            .tier(tier)
            .unwrap()
            .shards
            .iter()
            .enumerate()
            .map(|(ordinal, owner)| {
                let artifact = manifest.artifact_at(role, ordinal as u32).unwrap();
                let graph_path = directory.join(&artifact.relative_path);
                let (_, receipt) =
                    ValidatedNativeHnsw::load(Arc::clone(owner), &graph_path).unwrap();
                Some(SemanticAnnExpectation {
                    graph_path,
                    receipt,
                })
            })
            .collect::<Vec<_>>()
    };
    let fast = expectations(TierKind::Fast, SemanticArtifactRole::FastAnn);
    let quality = expectations(TierKind::Quality, SemanticArtifactRole::QualityAnn);
    assert_eq!(fast.len(), 3);
    assert_eq!(quality.len(), 3);
    let total = fast
        .iter()
        .chain(&quality)
        .flatten()
        .map(|expectation| expectation.receipt.graph_byte_len)
        .sum::<u64>();
    assert!(total > 0);
    let admitted = reader.clone().with_ann_budget(
        Some(&fast),
        Some(&quality),
        AnnAdmissionBudget {
            max_declared_graph_bytes: total,
        },
    )?;
    for tier in [TierKind::Fast, TierKind::Quality] {
        for ordinal in 0..3 {
            assert!(matches!(
                admitted.ann_admission(tier, ordinal),
                Some(SemanticAnnAdmission::Admitted { .. })
            ));
        }
    }
    for expected in fast.iter().chain(&quality).flatten() {
        fs::rename(
            &expected.graph_path,
            expected.graph_path.with_extension("retained"),
        )?;
    }
    let before = snapshot(root.path());
    let refused = reader.with_ann_budget(
        Some(&fast),
        Some(&quality),
        AnnAdmissionBudget {
            max_declared_graph_bytes: total - 1,
        },
    )?;
    for tier in [TierKind::Fast, TierKind::Quality] {
        for ordinal in 0..3 {
            assert_eq!(
                refused.ann_admission(tier, ordinal),
                Some(SemanticAnnAdmission::Unavailable {
                    reason: AnnFallbackReason::AdmissionBudget,
                })
            );
        }
    }
    let batch = refused
        .activate(&queries())?
        .search_with_ann(3, None, native_policy())?;
    assert_eq!(batch.hits()[0].document.message_id, 3);
    assert!(batch.execution().iter().all(|report| {
        report.engine == SemanticShardEngine::ExactFallback && report.ann_windows == 0
    }));
    assert_eq!(before, snapshot(root.path()));
    Ok(())
}
