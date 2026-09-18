//! Admission must bound work before opening artifacts, then retain its owners.
//! Real published FSVI images, no model or archive-performance simulation.

use super::*;

#[test]
fn budget_preflight_precedes_missing_vector_io() -> TestResult {
    let root = tempfile::tempdir()?;
    let manifest = fixture(root.path(), "budget-before-io", 1, Some(A), None);
    select(root.path(), &manifest, None);
    let artifact = &manifest.artifacts[0];
    let path = manifest
        .generation_dir(root.path())?
        .join(&artifact.relative_path);
    fs::rename(&path, path.with_extension("retained"))?;
    let before = snapshot(root.path());
    let result = SelectedSemanticGeneration::open_current(
        root.path(),
        &manifest.corpus,
        SemanticSelectionBudget {
            max_declared_vector_bytes: artifact.size_bytes - 1,
        },
    );
    assert!(
        matches!(result, Err(SemanticSelectionError::BudgetExceeded)),
        "budget rejection must precede artifact I/O, got {result:?}"
    );
    assert_eq!(snapshot(root.path()), before);
    Ok(())
}

#[test]
fn final_checkpoint_uses_retained_vectors_after_path_replacement() -> TestResult {
    let root = tempfile::tempdir()?;
    let manifest = fixture(root.path(), "retained-checkpoint", 1, Some(A), Some(B));
    let pointer = select(root.path(), &manifest, None);
    let reader = SelectedSemanticGeneration::open_current_with_checkpoint(
        root.path(),
        &manifest.corpus,
        SemanticSelectionBudget::default(),
        || {
            // Both vectors have been admitted. Rechecking the selection must
            // not re-read these mutable paths or lose their sealed owners.
            for artifact in &manifest.artifacts {
                let path = manifest
                    .generation_dir(root.path())
                    .unwrap()
                    .join(&artifact.relative_path);
                fs::rename(&path, path.with_extension("retained")).unwrap();
            }
        },
    )?;
    let before = snapshot(root.path());
    assert_eq!(reader.selection().pointer(), &pointer);
    assert_eq!(first_id(&reader.activate(&queries())?.search(1, None)?), 1);
    let quality = TieredQueryEmbeddings::quality_only(query(SemanticArtifactRole::QualityVector));
    assert_eq!(first_id(&reader.activate(&quality)?.search(1, None)?), 2);
    assert_eq!(snapshot(root.path()), before);
    // This is retained-byte serving, never permission for a fresh process to
    // treat missing artifacts as validated.
    assert!(
        SelectedSemanticGeneration::open_current(
            root.path(),
            &manifest.corpus,
            SemanticSelectionBudget::default(),
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn total_tier_budget_is_checked_before_any_artifact_io() -> TestResult {
    let root = tempfile::tempdir()?;
    let manifest = fixture(root.path(), "aggregate-preflight", 1, Some(A), Some(B));
    select(root.path(), &manifest, None);
    let total: u64 = manifest
        .artifacts
        .iter()
        .map(|artifact| artifact.size_bytes)
        .sum();
    SelectedSemanticGeneration::open_current(
        root.path(),
        &manifest.corpus,
        SemanticSelectionBudget {
            max_declared_vector_bytes: total,
        },
    )?;
    let first = manifest
        .generation_dir(root.path())?
        .join(&manifest.artifacts[0].relative_path);
    fs::rename(&first, first.with_extension("retained"))?;
    let result = SelectedSemanticGeneration::open_current(
        root.path(),
        &manifest.corpus,
        SemanticSelectionBudget {
            max_declared_vector_bytes: total - 1,
        },
    );
    assert!(matches!(
        result,
        Err(SemanticSelectionError::BudgetExceeded)
    ));
    Ok(())
}

#[test]
fn caller_corpus_mismatch_precedes_budget_and_artifact_reads() -> TestResult {
    let root = tempfile::tempdir()?;
    let manifest = fixture(root.path(), "corpus-before-budget", 1, Some(A), None);
    select(root.path(), &manifest, None);
    let path = manifest
        .generation_dir(root.path())?
        .join(&manifest.artifacts[0].relative_path);
    fs::rename(&path, path.with_extension("retained"))?;
    let mut foreign = manifest.corpus.clone();
    foreign.source_change_sequence += 1;
    assert!(matches!(
        SelectedSemanticGeneration::open_current(
            root.path(),
            &foreign,
            SemanticSelectionBudget {
                max_declared_vector_bytes: 0
            }
        ),
        Err(SemanticSelectionError::Publication(
            SemanticGenerationError::StaleCorpus { .. }
        ))
    ));
    Ok(())
}

#[test]
fn manifest_authentication_precedes_budget_rejection() -> TestResult {
    let root = tempfile::tempdir()?;
    let mut manifest = fixture(root.path(), "manifest-before-budget", 1, Some(A), None);
    select(root.path(), &manifest, None);
    manifest.build_id.push_str("-changed");
    fs::write(
        manifest.manifest_path(root.path())?,
        serde_json::to_vec(&manifest)?,
    )?;
    assert!(matches!(
        SelectedSemanticGeneration::open_current(
            root.path(),
            &manifest.corpus,
            SemanticSelectionBudget {
                max_declared_vector_bytes: 0
            }
        ),
        Err(SemanticSelectionError::Publication(
            SemanticGenerationError::ManifestDigestMismatch { .. }
        ))
    ));
    Ok(())
}

#[test]
fn final_checkpoint_still_authenticates_manifest_bytes() -> TestResult {
    let root = tempfile::tempdir()?;
    let manifest = fixture(root.path(), "manifest-recheck", 1, Some(A), None);
    select(root.path(), &manifest, None);
    let result = SelectedSemanticGeneration::open_current_with_checkpoint(
        root.path(),
        &manifest.corpus,
        SemanticSelectionBudget::default(),
        || {
            let mut changed = manifest.clone();
            changed.build_id.push_str("-changed");
            fs::write(
                changed.manifest_path(root.path()).unwrap(),
                serde_json::to_vec(&changed).unwrap(),
            )
            .unwrap();
        },
    );
    assert!(matches!(
        result,
        Err(SemanticSelectionError::Publication(
            SemanticGenerationError::ManifestDigestMismatch { .. }
        ))
    ));
    Ok(())
}

#[test]
fn final_checkpoint_still_rejects_malformed_current_pointer() -> TestResult {
    let root = tempfile::tempdir()?;
    let manifest = fixture(root.path(), "pointer-recheck", 1, Some(A), None);
    select(root.path(), &manifest, None);
    let result = SelectedSemanticGeneration::open_current_with_checkpoint(
        root.path(),
        &manifest.corpus,
        SemanticSelectionBudget::default(),
        || {
            fs::write(SemanticCurrentPointerV1::path(root.path()), b"{unfinished").unwrap();
        },
    );
    assert!(matches!(
        result,
        Err(SemanticSelectionError::Publication(
            SemanticGenerationError::PointerParse { .. }
        ))
    ));
    Ok(())
}

#[test]
fn full_disk_audit_still_validates_artifacts_after_reader_admission() -> TestResult {
    let root = tempfile::tempdir()?;
    let manifest = fixture(root.path(), "full-audit", 1, Some(A), None);
    select(root.path(), &manifest, None);
    let reader = open(root.path(), &manifest);
    load_current_semantic_generation(root.path(), Some(&manifest.corpus))?;
    let path = manifest
        .generation_dir(root.path())?
        .join(&manifest.artifacts[0].relative_path);
    fs::rename(&path, path.with_extension("retained"))?;
    assert!(load_current_semantic_generation(root.path(), Some(&manifest.corpus)).is_err());
    assert_eq!(first_id(&reader.activate(&queries())?.search(1, None)?), 1);
    Ok(())
}
