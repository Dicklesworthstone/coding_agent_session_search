//! Session membership is enforced before vector top-k selection. The archive
//! connection and its read-snapshot guard stay owned by the parent search
//! client; this module never opens another database or reads message bodies.

use super::*;

// Session scope must constrain vector selection, not just the hydrated page.
// Otherwise unrelated sessions can consume both bounded candidate windows and
// turn a valid chained search into an empty result.
const SEMANTIC_SESSION_SCOPE_MAX_PATHS: usize = 1_024;
const SEMANTIC_SESSION_SCOPE_MAX_CONVERSATIONS: usize = 4_096;
pub(super) const SEMANTIC_SESSION_SCOPE_MAX_MESSAGES: usize = 65_536;
const SEMANTIC_SESSION_SCOPE_PATH_BATCH: usize = 64;

/// Intersect authoritative session membership with the existing vector filters.
/// This wrapper deliberately does not widen agent, source, role, or time scope.
pub(super) struct SessionScopedSemanticFilter<'a> {
    pub(super) metadata: &'a SemanticFilter,
    pub(super) message_ids: &'a HashSet<u64>,
}

impl FsSearchFilter for SessionScopedSemanticFilter<'_> {
    fn matches(&self, doc_id: &str, metadata: Option<&serde_json::Value>) -> bool {
        // Reject most unrelated records without decoding the optional content
        // hash. The existing filter still validates the remaining ID fields.
        let message_id = doc_id
            .strip_prefix("m|")
            .and_then(|rest| rest.split_once('|'))
            .and_then(|(message_id, rest)| {
                // An invalid chunk would be dropped during hydration; reject
                // it before it can consume a valid candidate's top-k slot.
                let (chunk, _) = rest.split_once('|')?;
                chunk.parse::<u8>().ok()?;
                message_id.parse::<u64>().ok()
            });
        message_id.is_some_and(|id| self.message_ids.contains(&id))
            && self.metadata.matches(doc_id, metadata)
    }

    fn matches_doc_id_hash(
        &self,
        _doc_id_hash: u64,
        _metadata: Option<&serde_json::Value>,
    ) -> Option<bool> {
        // A hash does not prove membership in the selected archive sessions.
        None
    }

    fn name(&self) -> &str {
        "cass_session_scoped_semantic_filter"
    }
}

/// Read IDs only, in one archive snapshot, without hydrating session contents.
/// All limits fail explicitly; a truncated allowlist must never look complete.
/// Row/allocation bounds are not a deadline for an individual engine query.
pub(super) fn load_semantic_session_message_ids(
    conn: &SearchSqliteConnection,
    session_paths: &HashSet<String>,
    max_messages: usize,
) -> Result<HashSet<u64>> {
    if session_paths.len() > SEMANTIC_SESSION_SCOPE_MAX_PATHS {
        bail!(
            "semantic session scope exceeds {} paths; narrow the session selection or use lexical search",
            SEMANTIC_SESSION_SCOPE_MAX_PATHS
        );
    }
    if session_paths.is_empty() {
        return Ok(HashSet::new());
    }
    let max_messages = max_messages.min(SEMANTIC_SESSION_SCOPE_MAX_MESSAGES);
    let mut paths = session_paths.iter().map(String::as_str).collect::<Vec<_>>();
    paths.sort_unstable();
    let mut transaction = SearchReadTransaction::begin(conn)?;
    let mut conversation_ids = HashSet::new();
    for chunk in paths.chunks(SEMANTIC_SESSION_SCOPE_PATH_BATCH) {
        let remaining = SEMANTIC_SESSION_SCOPE_MAX_CONVERSATIONS - conversation_ids.len();
        // Exact, case-sensitive path equality is the same contract used by
        // postprocess_hits_page_core. Path text is always parameterized.
        let sql = format!(
            "SELECT id FROM conversations WHERE source_path COLLATE BINARY IN ({}) LIMIT {}",
            sql_placeholders(chunk.len()),
            remaining + 1
        );
        let params = chunk
            .iter()
            .map(|path| ParamValue::from(*path))
            .collect::<Vec<_>>();
        let rows: Vec<i64> = transaction.query_map_collect(&sql, &params, |row| row.get_typed(0))?;
        conversation_ids.extend(rows);
        if conversation_ids.len() > SEMANTIC_SESSION_SCOPE_MAX_CONVERSATIONS {
            bail!(
                "semantic session scope exceeds {} conversations; narrow the session selection or use lexical search",
                SEMANTIC_SESSION_SCOPE_MAX_CONVERSATIONS
            );
        }
    }

    let mut conversation_ids = conversation_ids.into_iter().collect::<Vec<_>>();
    conversation_ids.sort_unstable();
    let mut message_ids = HashSet::new();
    for conversation_id in conversation_ids {
        let remaining = max_messages - message_ids.len();
        // A single equality uses the existing conversation-message lookup
        // rather than materializing every archive message or its contents.
        let sql = format!(
            "SELECT id FROM messages WHERE conversation_id = ? ORDER BY id LIMIT {}",
            remaining + 1
        );
        let rows: Vec<i64> = transaction.query_map_collect(
            &sql,
            &[ParamValue::from(conversation_id)],
            |row| row.get_typed(0),
        )?;
        // The SQL mapper returns FrankenError; ID range validation returns
        // io::Error. Convert outside the mapper through this function's
        // anyhow::Result rather than mixing the two error types.
        for message_id in rows {
            message_ids.insert(semantic_message_id_from_db(message_id)?);
        }
        if message_ids.len() > max_messages {
            bail!(
                "semantic session scope exceeds {max_messages} messages; narrow the session selection or use lexical search"
            );
        }
    }
    transaction.rollback()?;
    Ok(message_ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::vector_index::{Quantization, SemanticDocId};

    fn archive() -> SearchSqliteFixture {
        let conn = SearchSqliteFixture::in_memory().expect("open scope archive");
        conn.execute_batch(
            "CREATE TABLE conversations (id INTEGER PRIMARY KEY, source_path TEXT NOT NULL);
             CREATE TABLE messages (id INTEGER PRIMARY KEY, conversation_id INTEGER NOT NULL);
             CREATE INDEX scope_conversation_path ON conversations(source_path);
             CREATE INDEX scope_message_conversation ON messages(conversation_id);",
        )
        .expect("scope schema");
        conn
    }

    fn insert_conversation(conn: &SearchSqliteFixture, id: i64, path: &str) {
        conn.execute_compat(
            "INSERT INTO conversations (id, source_path) VALUES (?, ?)",
            &[ParamValue::from(id), ParamValue::from(path)],
        )
        .expect("insert scope conversation");
    }

    fn insert_message(conn: &SearchSqliteFixture, id: i64, conversation: i64) {
        conn.execute_compat(
            "INSERT INTO messages (id, conversation_id) VALUES (?, ?)",
            &[ParamValue::from(id), ParamValue::from(conversation)],
        )
        .expect("insert scope message");
    }

    fn client(conn: SearchSqliteFixture) -> SearchClient {
        // Exercise the actual dedicated-owner connection and candidate method;
        // no lexical assets, model inference, or provider scans are needed.
        SearchClient {
            reader: None,
            sqlite: Mutex::new(Some(conn.into_connection())),
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
                "semantic-scope-test:{}",
                SEARCH_CLIENT_INSTANCE_COUNTER.fetch_add(1, Ordering::Relaxed)
            ),
            semantic: Mutex::new(None),
            last_tantivy_total_count: Mutex::new(None),
            last_lexical_degrade_reason: Mutex::new(None),
        }
    }

    fn id(message: u64, source: u32) -> String {
        SemanticDocId {
            message_id: message,
            chunk_idx: 0,
            agent_id: 1,
            workspace_id: 2,
            source_id: source,
            role: 1,
            created_at_ms: 100,
            content_hash: None,
        }
        .to_doc_id_string()
    }

    #[test]
    fn scope_membership_intersects_every_existing_metadata_constraint() {
        let base = SemanticFilter {
            agents: Some(HashSet::from([1])),
            workspaces: Some(HashSet::from([2])),
            sources: Some(HashSet::from([3])),
            roles: Some(HashSet::from([1])),
            created_from: Some(100),
            created_to: Some(100),
        };
        let message_ids = HashSet::from([42]);
        let filter = SessionScopedSemanticFilter {
            metadata: &base,
            message_ids: &message_ids,
        };
        assert!(filter.matches(&id(42, 3), None));
        for rejected in [
            "m|43|0|1|2|3|1|100",
            "m|42|0|9|2|3|1|100",
            "m|42|0|1|9|3|1|100",
            "m|42|0|1|2|9|1|100",
            "m|42|0|1|2|3|0|100",
            "m|42|0|1|2|3|1|99",
            "m|42|0|1|2|3|1|101",
            "m|-1|0|1|2|3|1|100",
            "m|18446744073709551616|0|1|2|3|1|100",
            "m|42|garbled",
            "m|42|256|1|2|3|1|100",
            "m|42|bad|1|2|3|1|100",
            "42",
        ] {
            assert!(!filter.matches(rejected, None), "admitted {rejected}");
        }
        assert_eq!(filter.matches_doc_id_hash(42, None), None);
        let empty = HashSet::new();
        let empty_filter = SessionScopedSemanticFilter {
            metadata: &base,
            message_ids: &empty,
        };
        assert!(!empty_filter.matches(&id(42, 3), None));
    }

    #[test]
    fn session_path_lookup_is_literal_case_sensitive_and_retains_distinct_conversations() {
        let conn = archive();
        let path = "/private/δ/one' OR 1=1 -- %_.jsonl";
        insert_conversation(&conn, 1, path);
        insert_conversation(&conn, 2, path);
        insert_conversation(&conn, 3, "/private/δ/ONE' OR 1=1 -- %_.jsonl");
        insert_conversation(&conn, 4, "/unrelated.jsonl");
        for n in 1..=4 {
            insert_message(&conn, n, n);
        }
        let selected = HashSet::from([path.to_string()]);
        assert_eq!(
            load_semantic_session_message_ids(conn.connection(), &selected, 10).unwrap(),
            HashSet::from([1, 2])
        );
        assert!(
            load_semantic_session_message_ids(
                conn.connection(),
                &HashSet::from(["/absent".into()]),
                10,
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn path_batches_preserve_every_selected_message() {
        let conn = archive();
        let mut paths = HashSet::new();
        for n in 1..=70_i64 {
            let path = format!("/scope/{n}.jsonl");
            insert_conversation(&conn, n, &path);
            insert_message(&conn, n, n);
            paths.insert(path);
        }
        let ids = load_semantic_session_message_ids(conn.connection(), &paths, 70).unwrap();
        assert_eq!(ids, (1..=70_u64).collect());
    }

    #[test]
    fn an_exceeded_message_limit_fails_without_a_partial_allowlist_or_open_transaction() {
        let conn = archive();
        insert_conversation(&conn, 1, "/scope");
        insert_message(&conn, 1, 1);
        insert_message(&conn, 2, 1);
        let paths = HashSet::from(["/scope".into()]);
        let error = load_semantic_session_message_ids(conn.connection(), &paths, 1).unwrap_err();
        assert!(error.to_string().contains("exceeds 1 messages"));
        // A fresh transaction must work after the early-error rollback.
        assert_eq!(
            load_semantic_session_message_ids(conn.connection(), &paths, 2).unwrap(),
            HashSet::from([1, 2])
        );
    }

    #[test]
    fn an_exceeded_path_limit_fails_before_querying_the_archive() {
        let conn = SearchSqliteFixture::in_memory().unwrap(); // deliberately no schema
        let paths = (0..=SEMANTIC_SESSION_SCOPE_MAX_PATHS)
            .map(|n| format!("/scope/{n}"))
            .collect();
        let error = load_semantic_session_message_ids(conn.connection(), &paths, 1).unwrap_err();
        assert!(error.to_string().contains("exceeds 1024 paths"));
    }

    #[test]
    fn scoped_candidate_search_recovers_matches_below_the_global_window() {
        let conn = archive();
        insert_conversation(&conn, 1, "/unrelated");
        insert_conversation(&conn, 2, "/selected");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("selected-generation.fsvi");
        let mut writer = FsVectorIndex::create_with_revision(
            &path,
            "fnv1a-2",
            "scope-test",
            2,
            Quantization::F16,
        )
        .unwrap();
        for n in 1..=128_i64 {
            insert_message(&conn, n, 1);
            writer.write_record(&id(n as u64, 3), &[1.0, 0.0]).unwrap();
        }
        insert_message(&conn, 129, 2);
        insert_message(&conn, 130, 2);
        writer.write_record(&id(129, 3), &[0.6, 0.8]).unwrap();
        writer.write_record(&id(130, 4), &[0.3, 0.9539392]).unwrap();
        writer.finish().unwrap();
        let before = std::fs::read(&path).unwrap();
        let artifact = SemanticIndexArtifact::open(&path, None).unwrap();
        let global = artifact
            .index()
            .search_top_k(&[1.0, 0.0], 24, None)
            .unwrap();
        assert!(global.iter().all(|hit| {
            parse_semantic_doc_id(&hit.doc_id).unwrap().message_id < 129
        }));
        let context = SemanticCandidateContext {
            artifacts: Arc::new(vec![artifact]),
            filter_maps: SemanticFilterMaps::for_tests(
                HashMap::new(),
                HashMap::new(),
                HashMap::from([("remote".into(), 3), ("local".into(), 4)]),
                HashSet::from([3]),
            ),
            roles: None,
        };
        let client = client(conn);
        let filters = SearchFilters {
            session_paths: HashSet::from(["/selected".into()]),
            ..Default::default()
        };
        for approximate in [false, true] {
            let (hits, retry, stats) = client.search_semantic_candidates(
                &context,
                &[1.0, 0.0],
                &filters,
                SemanticCandidateSearchRequest {
                    fetch_limit: 2,
                    approximate,
                    tier_mode: SemanticTierMode::Single,
                    in_memory_two_tier_index: None,
                    ann_index: None,
                },
            )
            .unwrap();
            assert_eq!(
                hits.iter().map(|hit| hit.message_id).collect::<Vec<_>>(),
                vec![129, 130]
            );
            assert!(!retry.has_more_candidates);
            assert!(!retry.exact_window_may_omit_competitor);
            assert!(
                stats.is_none(),
                "exact scope fallback must not claim ANN execution"
            );
        }
        let remote_only = SearchFilters {
            source_filter: SourceFilter::SourceId("remote".into()),
            ..filters
        };
        let (hits, _, _) = client.search_semantic_candidates(
            &context,
            &[1.0, 0.0],
            &remote_only,
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
            vec![129]
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn invalid_archive_id_rejects_scope_and_rolls_back_the_snapshot() {
        let conn = archive();
        insert_conversation(&conn, 1, "/negative");
        insert_message(&conn, -1, 1);
        let selected = HashSet::from(["/negative".to_string()]);
        let error = load_semantic_session_message_ids(conn.connection(), &selected, 10).unwrap_err();
        assert!(error.to_string().contains("negative message_id"));
        // A failed conversion must leave no transaction behind. A separate
        // missing session still has a complete, empty membership snapshot.
        assert!(load_semantic_session_message_ids(
            conn.connection(), &HashSet::from(["/missing".into()]), 10,
        ).unwrap().is_empty());
    }

    #[test]
    fn zero_message_budget_accepts_empty_sessions_but_not_a_partial_scope() {
        let conn = archive();
        insert_conversation(&conn, 1, "/empty");
        insert_conversation(&conn, 2, "/one-message");
        insert_message(&conn, 1, 2);
        let empty = HashSet::from(["/empty".to_string()]);
        assert!(load_semantic_session_message_ids(conn.connection(), &empty, 0)
            .unwrap().is_empty());
        let populated = HashSet::from(["/one-message".to_string()]);
        let error = load_semantic_session_message_ids(conn.connection(), &populated, 0).unwrap_err();
        assert!(error.to_string().contains("exceeds 0 messages"));
        assert_eq!(
            load_semantic_session_message_ids(conn.connection(), &populated, 1).unwrap(),
            HashSet::from([1]),
        );
    }

    #[test]
    fn scoped_exact_search_merges_shards_and_missing_session_never_broadens() {
        let conn = archive();
        insert_conversation(&conn, 1, "/selected");
        insert_conversation(&conn, 2, "/unrelated");
        insert_message(&conn, 1, 1);
        insert_message(&conn, 2, 1);
        insert_message(&conn, 3, 2);
        let dir = tempfile::tempdir().unwrap();
        let mut artifacts = Vec::new();
        for (ordinal, selected_id, vector) in [
            (0, 1_u64, [0.6, 0.8]),
            (1, 2_u64, [0.8, 0.6]),
        ] {
            let path = dir.path().join(format!("shard-{ordinal}.fsvi"));
            let mut writer = FsVectorIndex::create_with_revision(
                &path, "fnv1a-2", "scope-shards", 2, Quantization::F16,
            ).unwrap();
            writer.write_record(&id(3, 3), &[1.0, 0.0]).unwrap();
            writer.write_record(&id(selected_id, 3), &vector).unwrap();
            writer.finish().unwrap();
            artifacts.push(SemanticIndexArtifact::open(&path, None).unwrap());
        }
        let context = SemanticCandidateContext {
            artifacts: Arc::new(artifacts),
            filter_maps: SemanticFilterMaps::for_tests(
                HashMap::new(), HashMap::new(), HashMap::new(), HashSet::new(),
            ),
            roles: None,
        };
        let client = client(conn);
        for (path, expected) in [("/selected", vec![2, 1]), ("/missing", vec![])] {
            let (hits, retry, stats) = client.search_semantic_candidates(
                &context,
                &[1.0, 0.0],
                &SearchFilters {
                    session_paths: HashSet::from([path.to_string()]),
                    ..Default::default()
                },
                SemanticCandidateSearchRequest {
                    fetch_limit: 2,
                    approximate: true,
                    tier_mode: SemanticTierMode::Single,
                    in_memory_two_tier_index: None,
                    ann_index: None,
                },
            ).unwrap();
            assert_eq!(hits.iter().map(|hit| hit.message_id).collect::<Vec<_>>(), expected);
            assert!(!retry.has_more_candidates);
            assert!(!retry.exact_window_may_omit_competitor);
            assert!(stats.is_none());
        }
    }
}
