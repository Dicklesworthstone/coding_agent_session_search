//! Real FSVI/WAL ownership-transfer tests for backfill reconciliation.
use super::*;

fn snapshot_source(
    indexer: &SemanticIndexer,
    root: &Path,
    inputs: &[EmbeddingInput],
) -> Result<(PathBuf, Vec<EmbeddedMessage>)> {
    let path = root.join("source.fsvi");
    let embedded = indexer.embed_messages(inputs)?;
    drop(indexer.build_and_save_index_at_path(embedded.clone(), &path)?);
    Ok((path, embedded))
}

fn private_snapshot(source: &Path, root: &Path) -> Result<tempfile::TempDir> {
    let directory = tempfile::Builder::new().prefix(".owned-reconcile-test-").tempdir_in(root)?;
    let candidate = directory.path().join("candidate.fsvi");
    fs::copy(source, &candidate)?;
    let wal = fsvi_wal_path_for(source);
    if wal.exists() {
        fs::copy(wal, fsvi_wal_path_for(&candidate))?;
    }
    Ok(directory)
}

fn vector_signature(index: &FsVectorIndex) -> Result<std::collections::BTreeMap<String, Vec<u32>>> {
    let mut output = std::collections::BTreeMap::new();
    for row in 0..index.record_count() {
        anyhow::ensure!(!index.is_deleted(row), "test output has a tombstone");
        let id = index.doc_id_at(row)?.to_string();
        let vector = index.vector_at_f32(row)?.into_iter().map(f32::to_bits).collect();
        anyhow::ensure!(output.insert(id, vector).is_none(), "duplicate test output identity");
    }
    Ok(output)
}

#[test]
#[cfg(unix)]
fn owned_snapshot_transfers_the_same_inode_instead_of_copying_it_again() -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let root = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let inputs = [EmbeddingInput::new(1, "retained original vector")];
    let (source, embedded) = snapshot_source(&indexer, root.path(), &inputs)?;
    let source_before = fs::read(&source)?;
    let ids = embedded.iter().map(semantic_doc_id_for_embedded).collect::<HashSet<_>>();
    let directory = private_snapshot(&source, root.path())?;
    let scratch_path = directory.path().to_path_buf();
    let private_file = scratch_path.join("candidate.fsvi");
    let private_identity = fs::metadata(&private_file)?;
    let source_identity = fs::metadata(&source)?;
    assert_ne!((private_identity.dev(), private_identity.ino()), (source_identity.dev(), source_identity.ino()),
        "the first snapshot must be a real copy, not a link to published bytes");
    let output = root.path().join("owned.fsvi");
    let owned = indexer.reconcile_in_private_directory(Vec::new(), &output, &ids, || Ok(directory))?;
    let output_identity = fs::metadata(&output)?;
    assert_eq!((output_identity.dev(), output_identity.ino()), (private_identity.dev(), private_identity.ino()),
        "an unchanged private candidate must be transferred, not cloned into another staging file");
    assert!(!scratch_path.exists());
    assert_eq!(fs::read(&source)?, source_before);
    assert_eq!(fs::read(&output)?, source_before);

    // The ordinary live-source entry point MUST copy even for a no-op input.
    let copied_path = root.path().join("copied.fsvi");
    let copied = indexer.reconcile_index_at_paths(Vec::new(), &source, &copied_path,
        TierKind::Fast, "snapshot-control", &ids)?;
    let copied_identity = fs::metadata(&copied_path)?;
    assert_ne!((copied_identity.dev(), copied_identity.ino()), (source_identity.dev(), source_identity.ino()));
    assert_eq!(vector_signature(&owned)?, vector_signature(&copied)?);
    assert_eq!(fs::read(&source)?, source_before);
    Ok(())
}

#[test]
fn owned_snapshot_rejects_invalid_replacements_before_preparing_files() -> Result<()> {
    use std::cell::Cell;

    let root = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let valid = indexer.embed_messages(&[EmbeddingInput::new(1, "valid replacement")])?;
    let ids = valid.iter().map(semantic_doc_id_for_embedded).collect::<HashSet<_>>();
    let mut wrong_dimension = valid[0].clone();
    wrong_dimension.embedding.pop();
    let mut nonfinite = valid[0].clone();
    nonfinite.embedding[0] = f32::NAN;
    let mut foreign = valid[0].clone();
    foreign.message_id = 99;
    for (replacement, expected) in [
        (vec![wrong_dimension], "dimension mismatch"),
        (vec![nonfinite], "non-finite embedding"),
        (vec![foreign], "not present in the canonical DB"),
        (vec![valid[0].clone(), valid[0].clone()], "duplicate replacement document"),
    ] {
        let prepared = Cell::new(false);
        let error = indexer.reconcile_in_private_directory(
            replacement, &root.path().join("must-not-exist.fsvi"), &ids,
            || { prepared.set(true); anyhow::bail!("preparation must not run"); },
        ).unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
        assert!(!prepared.get(), "invalid replacements reached snapshot I/O");
        assert_eq!(fs::read_dir(root.path())?.count(), 0);
    }
    Ok(())
}

#[test]
fn owned_snapshot_reconciles_edits_deletions_and_retained_vectors_exactly() -> Result<()> {
    let root = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let (source, original) = snapshot_source(&indexer, root.path(), &[
        EmbeddingInput::new(1, "retained vector content"),
        EmbeddingInput::new(2, "old edited content"),
        EmbeddingInput::new(3, "deleted content"),
    ])?;
    let before = fs::read(&source)?;
    let replacements = indexer.embed_messages(&[
        EmbeddingInput::new(2, "new edited content"),
        EmbeddingInput::new(4, "newly added content"),
    ])?;
    let ids = std::iter::once(semantic_doc_id_for_embedded(&original[0]))
        .chain(replacements.iter().map(semantic_doc_id_for_embedded)).collect::<HashSet<_>>();
    let expected_path = root.path().join("expected.fsvi");
    let mut complete = vec![original[0].clone()];
    complete.extend(replacements.clone());
    let expected = indexer.build_and_save_index_at_path(complete, &expected_path)?;
    let directory = private_snapshot(&source, root.path())?;
    let scratch_path = directory.path().to_path_buf();
    let output = indexer.reconcile_in_private_directory(
        replacements, &root.path().join("reconciled.fsvi"), &ids, || Ok(directory),
    )?;
    assert_eq!(vector_signature(&output)?, vector_signature(&expected)?);
    assert_eq!(output.wal_record_count(), 0);
    assert_eq!(output.tombstone_count(), 0);
    assert_eq!(output.record_count(), 3);
    assert!(!scratch_path.exists());
    assert_eq!(fs::read(source)?, before);
    Ok(())
}

#[test]
fn owned_snapshot_preserves_destination_and_cleans_scratch_on_coverage_failure() -> Result<()> {
    let root = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let (source, original) = snapshot_source(&indexer, root.path(), &[
        EmbeddingInput::new(1, "existing published vector"),
    ])?;
    let before = fs::read(&source)?;
    let missing = indexer.embed_messages(&[EmbeddingInput::new(2, "missing new vector")])?;
    let ids = original.iter().chain(&missing).map(semantic_doc_id_for_embedded).collect::<HashSet<_>>();
    let directory = private_snapshot(&source, root.path())?;
    let scratch_path = directory.path().to_path_buf();
    let error = indexer.reconcile_in_private_directory(Vec::new(), &source, &ids, || Ok(directory)).unwrap_err();
    assert!(error.to_string().contains("staging count mismatch"), "{error}");
    assert_eq!(fs::read(&source)?, before);
    assert!(!scratch_path.exists(), "failed reconciliation retained abandoned scratch");
    assert_eq!(FsVectorIndex::open_read_only(&source)?.record_count(), 1);
    Ok(())
}

#[test]
fn owned_snapshot_consumes_real_wal_without_replaying_it_after_publication() -> Result<()> {
    let root = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let (source, original) = snapshot_source(&indexer, root.path(), &[
        EmbeddingInput::new(1, "old original vector"),
        EmbeddingInput::new(2, "retained second vector"),
    ])?;
    let updates = indexer.embed_messages(&[
        EmbeddingInput::new(3, "durable WAL-only addition"),
    ])?;
    {
        let mut writer = FsVectorIndex::open_writer(&source)?;
        writer.append_batch(&updates.iter().map(|embedded|
            (semantic_doc_id_for_embedded(embedded), embedded.embedding.clone())).collect::<Vec<_>>())?;
        assert_eq!(writer.wal_record_count(), 1, "fixture must exercise retained WAL");
    }
    let source_wal = fsvi_wal_path_for(&source);
    let before = (fs::read(&source)?, fs::read(&source_wal)?);
    let ids = original.iter().chain(&updates).map(semantic_doc_id_for_embedded).collect::<HashSet<_>>();
    let directory = private_snapshot(&source, root.path())?;
    let scratch = directory.path().to_path_buf();
    let output_path = root.path().join("wal-reconciled.fsvi");
    let output = indexer.reconcile_in_private_directory(Vec::new(), &output_path, &ids, || Ok(directory))?;
    assert_eq!(output.live_doc_ids()?, ids);
    assert_eq!(output.record_count(), 3);
    assert_eq!(output.wal_record_count(), 0);
    let expected_signature = vector_signature(&output)?;
    drop(output);
    assert!(!scratch.exists());
    // This probe carries the exact old serialized WAL, not manufactured bytes.
    let probe = root.path().join("replay-probe.fsvi");
    fs::copy(&output_path, &probe)?;
    fs::copy(&source_wal, fsvi_wal_path_for(&probe))?;
    let reopened = FsVectorIndex::open_read_only(&probe)?;
    assert_eq!(reopened.wal_record_count(), 0);
    assert_eq!(vector_signature(&reopened)?, expected_signature);
    assert_eq!((fs::read(&source)?, fs::read(&source_wal)?), before);
    Ok(())
}

#[test]
fn owned_snapshot_can_replace_a_foreign_contract_only_with_complete_new_vectors() -> Result<()> {
    let root = tempfile::tempdir()?;
    let indexer = SemanticIndexer::new("hash", None)?;
    let embedded = indexer.embed_messages(&[EmbeddingInput::new(1, "current vector text")])?;
    let source = root.path().join("foreign.fsvi");
    let mut writer = FsVectorIndex::create_with_revision(&source, indexer.embedder_id(),
        "foreign-input-contract", indexer.embedder_dimension(), FsQuantization::F16)?;
    writer.write_record(&semantic_doc_id_for_embedded(&embedded[0]), &embedded[0].embedding)?;
    writer.finish()?;
    let before = fs::read(&source)?;
    let ids = embedded.iter().map(semantic_doc_id_for_embedded).collect::<HashSet<_>>();
    let rejected = private_snapshot(&source, root.path())?;
    let error = indexer.reconcile_in_private_directory(Vec::new(), &root.path().join("reject.fsvi"),
        &ids, || Ok(rejected)).unwrap_err();
    assert!(error.to_string().contains("incompatible with embedder"), "{error}");
    let accepted = private_snapshot(&source, root.path())?;
    let output = indexer.reconcile_in_private_directory(embedded, &root.path().join("replace.fsvi"),
        &ids, || Ok(accepted))?;
    assert_eq!(output.embedder_revision(), indexer.vector_space_revision()?);
    assert_eq!(output.live_doc_ids()?, ids);
    assert_eq!(fs::read(&source)?, before);
    Ok(())
}
