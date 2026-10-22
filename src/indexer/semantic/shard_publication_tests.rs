//! Real file-backed shard publication regressions. Hash vectors are a control
//! producer, not evidence of semantic relevance or native-model readiness.
use super::*;
use std::collections::BTreeMap;

fn indexer() -> SemanticIndexer {
    SemanticIndexer {
        embedder: Box::new(HashEmbedder::default()),
        batch_size: 2,
        exact_reuse: false,
        external_cancelled: None,
        replay_cache: replay_cache::CanonicalReplayCache::default(),
    }
}

fn plan(tier: TierKind, ann: bool) -> SemanticShardBuildPlan {
    SemanticShardBuildPlan {
        tier,
        db_fingerprint: "identity-v1:shard-test:content-v1:3:3:3".into(),
        model_revision: "hash".into(),
        total_conversations: 3,
        max_records_per_shard: 1,
        build_ann: ann,
    }
}

fn rows(indexer: &SemanticIndexer, text: &str) -> Vec<EmbeddedMessage> {
    (1..=3)
        .map(|message_id| EmbeddedMessage {
            message_id,
            created_at_ms: 100,
            agent_id: 1,
            workspace_id: 2,
            source_id: 3,
            role: ROLE_USER,
            chunk_idx: 0,
            content_hash: content_hash(text),
            embedding: indexer.embedder.embed_sync(text).unwrap(),
        })
        .collect()
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(snapshot(&path));
        } else {
            files.insert(path.clone(), fs::read(path).unwrap());
        }
    }
    files
}

fn assert_preserved(files: &BTreeMap<PathBuf, Vec<u8>>) {
    for (path, bytes) in files {
        assert_eq!(
            &fs::read(path).unwrap(),
            bytes,
            "published file changed: {}",
            path.display()
        );
    }
}

#[test]
fn repeated_source_fingerprint_publishes_new_paths_and_preserves_old_native_readers() {
    let temp = tempfile::tempdir().unwrap();
    let indexer = indexer();
    let old = indexer
        .build_and_save_index_shards(
            rows(&indexer, "old compiler storage"),
            temp.path(),
            plan(TierKind::Fast, true),
        )
        .unwrap();
    let old_files = snapshot(old.index_paths[0].parent().unwrap());
    let reader = FsVectorIndex::open_read_only(&old.index_paths[0]).unwrap();
    let query = indexer.embedder.embed_sync("old compiler storage").unwrap();
    let before = reader.search_top_k(&query, 1, None).unwrap();
    let new = indexer
        .build_and_save_index_shards(
            rows(&indexer, "new astronomy planets"),
            temp.path(),
            plan(TierKind::Fast, true),
        )
        .unwrap();
    assert!(old.complete && new.complete);
    assert!(
        new.index_paths
            .iter()
            .all(|path| !old.index_paths.contains(path)),
        "a rebuild must not reuse installed vector paths"
    );
    assert!(
        new.ann_index_paths
            .iter()
            .all(|path| !old.ann_index_paths.contains(path)),
        "a rebuild must not reuse installed ANN paths"
    );
    assert_preserved(&old_files);
    let after = reader.search_top_k(&query, 1, None).unwrap();
    assert_eq!(before[0].doc_id, after[0].doc_id);
    assert_eq!(before[0].score.to_bits(), after[0].score.to_bits());
    let manifest = SemanticShardManifest::load(temp.path()).unwrap().unwrap();
    for (ordinal, record) in manifest.shards.iter().enumerate() {
        let path = temp.path().join(&record.index_path);
        assert_eq!(path, new.index_paths[ordinal]);
        let source = FsVectorIndex::open_read_only(&path).unwrap();
        let graph = temp.path().join(record.ann_index_path.as_ref().unwrap());
        assert!(
            FsHnswIndex::try_load_native(&graph, &source)
                .unwrap()
                .is_some()
        );
    }
}

#[test]
fn failure_after_one_new_shard_keeps_every_installed_file_and_manifest_unchanged() {
    let temp = tempfile::tempdir().unwrap();
    let indexer = indexer();
    let old = indexer
        .build_and_save_index_shards(
            rows(&indexer, "old stable archive"),
            temp.path(),
            plan(TierKind::Fast, true),
        )
        .unwrap();
    let old_files = snapshot(old.index_paths[0].parent().unwrap());
    let manifest_before = fs::read(SemanticShardManifest::path(temp.path())).unwrap();
    let mut replacement = rows(&indexer, "replacement generation");
    replacement[1].embedding[0] = f32::NAN;
    let result =
        indexer.build_and_save_index_shards(replacement, temp.path(), plan(TierKind::Fast, true));
    assert!(result.is_err());
    assert_eq!(
        fs::read(SemanticShardManifest::path(temp.path())).unwrap(),
        manifest_before
    );
    assert_preserved(&old_files);
    let manifest = SemanticShardManifest::load(temp.path()).unwrap().unwrap();
    assert!(
        manifest
            .summary(
                TierKind::Fast,
                indexer.embedder_id(),
                &plan(TierKind::Fast, true).db_fingerprint
            )
            .complete
    );
}

#[test]
fn malformed_or_future_manifest_is_not_discarded_before_a_new_build() {
    for bytes in [
        b"not-json".as_slice(),
        br#"{"manifest_version":4294967295,"shards":[],"updated_at_ms":0}"#.as_slice(),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let path = SemanticShardManifest::path(temp.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        let before = snapshot(temp.path());
        let inputs = std::iter::from_fn(|| -> Option<EmbeddedMessage> {
            panic!("invalid manifest must reject before input consumption")
        });
        let result =
            indexer().build_and_save_index_shards(inputs, temp.path(), plan(TierKind::Fast, false));
        assert!(result.is_err());
        assert_eq!(snapshot(temp.path()), before);
    }
}

#[test]
fn corrupt_manifest_observed_at_publish_is_preserved_not_overwritten() {
    let temp = tempfile::tempdir().unwrap();
    let indexer = indexer();
    let path = SemanticShardManifest::path(temp.path());
    let mut input = rows(&indexer, "new vectors").into_iter();
    let mut changed = false;
    let replacement = std::iter::from_fn(|| {
        let next = input.next();
        if next.is_none() && !changed {
            fs::write(&path, b"externally changed invalid manifest").unwrap();
            changed = true;
        }
        next
    });
    assert!(
        indexer
            .build_and_save_index_shards(replacement, temp.path(), plan(TierKind::Fast, false))
            .is_err()
    );
    assert!(changed);
    assert_eq!(
        fs::read(&path).unwrap(),
        b"externally changed invalid manifest"
    );
}

#[test]
fn publishing_one_tier_keeps_the_other_tiers_selected_files() {
    let temp = tempfile::tempdir().unwrap();
    let indexer = indexer();
    let quality = indexer
        .build_and_save_index_shards(
            rows(&indexer, "quality control"),
            temp.path(),
            plan(TierKind::Quality, false),
        )
        .unwrap();
    let before = snapshot(quality.index_paths[0].parent().unwrap());
    let fast = indexer
        .build_and_save_index_shards(
            rows(&indexer, "fast control"),
            temp.path(),
            plan(TierKind::Fast, false),
        )
        .unwrap();
    assert!(fast.complete && quality.complete);
    let manifest = SemanticShardManifest::load(temp.path()).unwrap().unwrap();
    assert_eq!(
        manifest
            .shards
            .iter()
            .filter(|s| s.tier == TierKind::Quality)
            .map(|s| temp.path().join(&s.index_path))
            .collect::<Vec<_>>(),
        quality.index_paths
    );
    assert_preserved(&before);
}

#[test]
fn huge_shard_limit_does_not_allocate_before_receiving_a_small_input() {
    let temp = tempfile::tempdir().unwrap();
    let indexer = indexer();
    let mut plan = plan(TierKind::Fast, false);
    plan.max_records_per_shard = usize::MAX;
    let outcome = indexer
        .build_and_save_index_shards(rows(&indexer, "bounded inputs"), temp.path(), plan)
        .unwrap();
    assert_eq!(outcome.shard_count, 1);
    assert_eq!(outcome.doc_count, 3);
    assert!(outcome.complete);
}

// These tests exercise the direct writer used by normal indexing, not only
// the shard-building prototype. Every fixture is confined to a temporary root.
#[test]
fn direct_failed_rebuild_keeps_the_installed_index_and_progress_boundary() {
    let temp = tempfile::tempdir().unwrap();
    let indexer = indexer();
    drop(
        indexer
            .build_and_save_index(rows(&indexer, "installed compiler history"), temp.path())
            .unwrap(),
    );
    let path = vector_index_path(temp.path(), indexer.embedder_id());
    let before = snapshot(temp.path());
    let mut replacement = rows(&indexer, "replacement storage history");
    replacement[1].embedding[0] = f32::NAN;
    let mut progress = Vec::new();
    let outcome = indexer
        .build_and_save_index_with_progress(replacement, temp.path(), |count| progress.push(count));
    assert!(outcome.is_err());
    assert_eq!(progress, vec![1], "only accepted records report progress");
    assert!(
        path.is_file(),
        "failed direct rebuild removed the installed FSVI"
    );
    assert_eq!(snapshot(temp.path()), before);
    let reopened = FsVectorIndex::open_read_only(&path).unwrap();
    assert_eq!(reopened.record_count(), 3);
}

#[test]
fn direct_failed_rebuild_preserves_pending_wal_entries() {
    let temp = tempfile::tempdir().unwrap();
    let indexer = indexer();
    let mut original = indexer
        .build_and_save_index(rows(&indexer, "installed vectors"), temp.path())
        .unwrap();
    let mut pending = rows(&indexer, "acknowledged pending work").pop().unwrap();
    pending.message_id = 99;
    let pending_id = semantic_doc_id_for_embedded(&pending);
    original
        .append_batch(&[(pending_id.clone(), pending.embedding.clone())])
        .unwrap();
    assert_eq!(original.wal_record_count(), 1);
    drop(original);
    let path = vector_index_path(temp.path(), indexer.embedder_id());
    let before = snapshot(temp.path());
    let mut replacement = rows(&indexer, "bad replacement");
    replacement[1].embedding.pop();
    assert!(
        indexer
            .build_and_save_index(replacement, temp.path())
            .is_err()
    );
    assert!(
        path.is_file(),
        "failed direct rebuild removed the WAL's installed main"
    );
    assert_eq!(snapshot(temp.path()), before);
    let reopened = FsVectorIndex::open_read_only(&path).unwrap();
    assert_eq!(reopened.wal_record_count(), 1);
    assert!(
        reopened
            .search_top_k(&pending.embedding, 10, None)
            .unwrap()
            .iter()
            .any(|hit| hit.doc_id == pending_id)
    );
}

#[test]
fn direct_writer_rejection_preserves_typed_cause_and_installed_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let indexer = indexer();
    drop(
        indexer
            .build_and_save_index(rows(&indexer, "installed source"), temp.path())
            .unwrap(),
    );
    let before = snapshot(temp.path());
    let mut replacement = rows(&indexer, "new source");
    // Finite and correctly dimensioned, but the engine must reject zero norm.
    replacement[1].embedding.fill(0.0);
    let mut progress = Vec::new();
    let Err(error) =
        indexer.build_and_save_index_with_progress(replacement, temp.path(), |count| {
            progress.push(count)
        })
    else {
        panic!("zero norm must be rejected by the real writer");
    };
    assert!(
        matches!(
            error.downcast_ref::<frankensearch::SearchError>(),
            Some(frankensearch::SearchError::InvalidConfig { .. })
        ),
        "backend error must remain typed: {error:#}"
    );
    assert_eq!(progress, vec![1]);
    assert_eq!(snapshot(temp.path()), before);
}

#[test]
fn direct_finish_refusal_does_not_remove_or_replay_an_existing_wal() {
    let temp = tempfile::tempdir().unwrap();
    let indexer = indexer();
    let mut original = indexer
        .build_and_save_index(rows(&indexer, "original main"), temp.path())
        .unwrap();
    let mut pending = rows(&indexer, "original wal").pop().unwrap();
    pending.message_id = 99;
    original
        .append_batch(&[(semantic_doc_id_for_embedded(&pending), pending.embedding)])
        .unwrap();
    drop(original);
    let before = snapshot(temp.path());
    let Err(error) = indexer.build_and_save_index(rows(&indexer, "replacement main"), temp.path())
    else {
        panic!("ordinary writer must not discard an acknowledged WAL");
    };
    assert!(
        matches!(error.downcast_ref::<frankensearch::SearchError>(), Some(frankensearch::SearchError::InvalidConfig { field, .. }) if field == "wal_sidecar"),
        "preserve the engine's WAL refusal: {error:#}"
    );
    assert_eq!(snapshot(temp.path()), before);
}

#[test]
fn direct_progress_panic_does_not_publish_a_partial_replacement() {
    let temp = tempfile::tempdir().unwrap();
    let indexer = indexer();
    drop(
        indexer
            .build_and_save_index(rows(&indexer, "original stable history"), temp.path())
            .unwrap(),
    );
    let before = snapshot(temp.path());
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = indexer.build_and_save_index_with_progress(
            rows(&indexer, "interrupted replacement"),
            temp.path(),
            |_| panic!("intentional progress interruption"),
        );
    }));
    assert!(outcome.is_err());
    assert_eq!(snapshot(temp.path()), before);
}

#[test]
fn direct_successful_rebuild_publishes_new_bytes_without_changing_an_open_reader() {
    let temp = tempfile::tempdir().unwrap();
    let indexer = indexer();
    let old_rows = rows(&indexer, "old compiler analysis");
    let query = old_rows[0].embedding.clone();
    drop(indexer.build_and_save_index(old_rows, temp.path()).unwrap());
    let path = vector_index_path(temp.path(), indexer.embedder_id());
    let old_bytes = fs::read(&path).unwrap();
    let retained = FsVectorIndex::open_read_only(&path).unwrap();
    let old_results = retained.search_top_k(&query, 3, None).unwrap();
    let new_rows = rows(&indexer, "new astronomy analysis");
    let expected: HashSet<_> = new_rows.iter().map(semantic_doc_id_for_embedded).collect();
    let mut progress = Vec::new();
    let replacement = indexer
        .build_and_save_index_with_progress(new_rows, temp.path(), |count| progress.push(count))
        .unwrap();
    assert_eq!(progress, vec![1, 2, 3]);
    assert_eq!(replacement.record_count(), 3);
    assert_eq!(replacement.wal_record_count(), 0);
    assert_ne!(fs::read(&path).unwrap(), old_bytes);
    let new_ids: HashSet<_> = replacement
        .search_top_k(&query, 3, None)
        .unwrap()
        .into_iter()
        .map(|hit| hit.doc_id.to_string())
        .collect();
    assert_eq!(new_ids, expected);
    let retained_results = retained.search_top_k(&query, 3, None).unwrap();
    for (before, after) in old_results.iter().zip(&retained_results) {
        assert_eq!(before.doc_id, after.doc_id);
        assert_eq!(before.score.to_bits(), after.score.to_bits());
    }
    assert_eq!(old_results.len(), retained_results.len());
}

#[test]
fn direct_invalid_first_build_leaves_no_published_artifact() {
    let temp = tempfile::tempdir().unwrap();
    let indexer = indexer();
    let mut input = rows(&indexer, "first build");
    input[1].embedding[0] = f32::INFINITY;
    assert!(indexer.build_and_save_index(input, temp.path()).is_err());
    assert!(!vector_index_path(temp.path(), indexer.embedder_id()).exists());
    assert!(snapshot(temp.path()).is_empty());
}
