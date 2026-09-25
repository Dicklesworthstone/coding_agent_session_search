//! SearchClient routing/cache coverage. These run in the complete CASS graph,
//! separately from the dependency-independent native-graph component harness.

use super::*;
use crate::search::hash_embedder::HashEmbedder;
use crate::search::vector_index::Quantization;
use frankensearch::index::HnswConfig;

fn client(connection: Option<SearchSqliteConnection>) -> SearchClient {
    SearchClient {
        reader: LexicalReaderSlot::default(),
        sqlite: Mutex::new(connection),
        sqlite_path: None,
        strict_read_only: true,
        prefix_cache: Mutex::new(CacheShards::new(128, 1024 * 1024)),
        reload_on_search: false,
        last_reload: Mutex::new(None),
        last_generation: Mutex::new(None),
        reload_epoch: Arc::new(AtomicU64::new(0)),
        warm_tx: None,
        _warm_handle: None,
        metrics: Metrics::default(),
        cache_namespace: format!(
            "ann-shards:{}",
            SEARCH_CLIENT_INSTANCE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ),
        semantic: Mutex::new(None),
        last_tantivy_total_count: Mutex::new(None),
        last_lexical_degrade_reason: Mutex::new(None),
        last_wildcard_fallback_skip: Mutex::new(None),
    }
}

fn artifact(dir: &Path, name: &str, message: u64, vector: [f32; 2]) -> SemanticIndexArtifact {
    let path = dir.join(format!("{name}.fsvi"));
    let ann = dir.join(format!("{name}.chsw"));
    let mut writer = FsVectorIndex::create_with_revision(
        &path,
        "fnv1a-2",
        "ann-routing-test",
        2,
        Quantization::F32,
    )
    .unwrap();
    let id = SemanticDocId {
        message_id: message,
        chunk_idx: 0,
        agent_id: 1,
        workspace_id: 2,
        source_id: 3,
        role: 1,
        created_at_ms: 100,
        content_hash: None,
    }
    .to_doc_id_string();
    writer.write_record(&id, &vector).unwrap();
    writer.finish().unwrap();
    let index = FsVectorIndex::open_read_only(&path).unwrap();
    FsHnswIndex::build_from_vector_index(&index, HnswConfig::default())
        .unwrap()
        .save(&ann)
        .unwrap();
    SemanticIndexArtifact::open(path, Some(ann)).unwrap()
}

fn install(
    client: &SearchClient,
    artifacts: Vec<SemanticIndexArtifact>,
) -> SemanticCandidateContext {
    client
        .set_semantic_artifacts_context(
            Arc::new(HashEmbedder::new(2)),
            artifacts,
            None,
            SemanticFilterMaps::for_tests(
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                HashSet::new(),
            ),
            None,
        )
        .unwrap();
    let guard = client.semantic.lock().unwrap();
    let state = guard.as_ref().unwrap();
    SemanticCandidateContext {
        artifacts: Arc::clone(&state.artifacts),
        filter_maps: state.filter_maps.clone(),
        roles: state.roles.clone(),
    }
}

#[test]
fn search_client_routes_a_complete_sharded_cohort_through_native_ann() {
    let dir = tempfile::tempdir().unwrap();
    let client = client(None);
    let context = install(
        &client,
        vec![
            artifact(dir.path(), "a", 1, [0.6, 0.8]),
            artifact(dir.path(), "b", 2, [1.0, 0.0]),
        ],
    );
    let ann = client
        .ann_index()
        .unwrap()
        .expect("complete shards must admit ANN");
    assert!(
        Arc::ptr_eq(&ann, &client.ann_index().unwrap().unwrap()),
        "reuse admitted native owners"
    );
    assert!(client.ann_unavailability_reason().unwrap().is_none());
    let (hits, _, stats) = client
        .search_semantic_candidates(
            &context,
            &[1.0, 0.0],
            &SearchFilters::default(),
            SemanticCandidateSearchRequest {
                fetch_limit: 2,
                approximate: true,
                tier_mode: SemanticTierMode::Single,
                in_memory_two_tier_index: None,
                ann_index: Some(&ann),
            },
        )
        .unwrap();
    assert_eq!(
        hits.iter().map(|hit| hit.message_id).collect::<Vec<_>>(),
        vec![2, 1]
    );
    assert_eq!(stats.unwrap().index_size, 2);
}

#[test]
fn missing_shard_ann_falls_back_to_every_exact_shard_not_just_ready_ones() {
    let dir = tempfile::tempdir().unwrap();
    let client = client(None);
    let first = artifact(dir.path(), "a", 1, [0.6, 0.8]);
    let second = artifact(dir.path(), "b", 2, [1.0, 0.0]);
    let second = SemanticIndexArtifact::open(second.fsvi_path(), None).unwrap();
    let context = install(&client, vec![first, second]);
    assert!(client.ann_index().unwrap().is_none());
    assert!(matches!(
        client.ann_unavailability_reason().unwrap(),
        Some(SemanticAnnUnavailableReason::SidecarMissing)
    ));
    let (hits, _, stats) = client
        .search_semantic_candidates(
            &context,
            &[1.0, 0.0],
            &SearchFilters::default(),
            SemanticCandidateSearchRequest {
                fetch_limit: 2,
                approximate: false,
                tier_mode: SemanticTierMode::Single,
                in_memory_two_tier_index: None,
                ann_index: None,
            },
        )
        .unwrap();
    assert_eq!(
        hits.iter().map(|hit| hit.message_id).collect::<Vec<_>>(),
        vec![2, 1]
    );
    assert!(stats.is_none(), "fallback is exact, not an ANN success");
}

#[test]
fn session_scoped_candidates_still_bypass_sharded_ann() {
    let dir = tempfile::tempdir().unwrap();
    let connection = SearchSqliteFixture::in_memory().unwrap();
    connection
        .execute_batch(
            "CREATE TABLE conversations (id INTEGER PRIMARY KEY, source_path TEXT NOT NULL);
        CREATE TABLE messages (id INTEGER PRIMARY KEY, conversation_id INTEGER NOT NULL);
        INSERT INTO conversations VALUES (1, '/selected'), (2, '/unrelated');
        INSERT INTO messages VALUES (1, 1), (2, 2);",
        )
        .unwrap();
    let client = client(Some(connection.into_connection()));
    let context = install(
        &client,
        vec![
            artifact(dir.path(), "a", 1, [0.6, 0.8]),
            artifact(dir.path(), "b", 2, [1.0, 0.0]),
        ],
    );
    let filters = SearchFilters {
        session_paths: HashSet::from(["/selected".to_string()]),
        ..Default::default()
    };
    let (hits, _, stats) = client
        .search_semantic_candidates(
            &context,
            &[1.0, 0.0],
            &filters,
            SemanticCandidateSearchRequest {
                fetch_limit: 2,
                approximate: true,
                tier_mode: SemanticTierMode::Single,
                in_memory_two_tier_index: None,
                ann_index: None,
            },
        )
        .unwrap();
    assert_eq!(
        hits.iter().map(|hit| hit.message_id).collect::<Vec<_>>(),
        vec![1]
    );
    assert!(stats.is_none());
    assert!(
        client
            .semantic
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .fs_ann_index
            .is_none()
    );
}

#[test]
fn replacing_the_context_invalidates_cached_ann_and_its_old_owner_identity() {
    let dir = tempfile::tempdir().unwrap();
    let client = client(None);
    let old = install(&client, vec![artifact(dir.path(), "old", 1, [0.6, 0.8])]);
    let old_ann = client.ann_index().unwrap().unwrap();
    let current = install(&client, vec![artifact(dir.path(), "new", 2, [1.0, 0.0])]);
    let new_ann = client.ann_index().unwrap().unwrap();
    assert!(!Arc::ptr_eq(&old_ann, &new_ann));
    assert!(
        old_ann
            .search(&current.artifacts, &[1.0, 0.0], 1, None)
            .is_err()
    );
    assert_eq!(
        old_ann
            .search(&old.artifacts, &[1.0, 0.0], 1, None)
            .unwrap()
            .0[0]
            .message_id,
        1
    );
    assert_eq!(
        new_ann
            .search(&current.artifacts, &[1.0, 0.0], 1, None)
            .unwrap()
            .0[0]
            .message_id,
        2
    );
    client.clear_semantic_context().unwrap();
    assert!(client.ann_index().is_err());
}
