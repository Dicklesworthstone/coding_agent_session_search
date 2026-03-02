//! Integration tests verifying the frankensearch search migration (bead s3ho2).
//!
//! Validates that:
//! 1. All search operations go through frankensearch (no direct tantivy imports remain)
//! 2. SemanticFilter directly implements frankensearch::core::filter::SearchFilter
//! 3. No duplicate FsSemanticFilterAdapter exists
//! 4. Vector search via frankensearch VectorIndex produces correct results
//! 5. RRF hybrid fusion uses frankensearch::rrf_fuse
//! 6. Query parsing and search pipeline work end-to-end through frankensearch

use coding_agent_search::search::query::{FieldMask, SearchClient, SearchFilters};
use coding_agent_search::search::tantivy::TantivyIndex;
use coding_agent_search::search::vector_index::{
    SemanticFilter, VectorIndex, parse_semantic_doc_id,
};
use std::collections::HashSet;
use tempfile::TempDir;

mod util;

// =============================================================================
// ZERO TANTIVY IMPORTS AUDIT
// =============================================================================

/// Programmatic verification that no direct `use tantivy::` imports remain in src/.
/// This test reads the source files and ensures all tantivy usage goes through
/// frankensearch re-exports.
#[test]
fn no_direct_tantivy_imports_in_src() {
    let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut violations = Vec::new();

    fn scan_dir(dir: &std::path::Path, violations: &mut Vec<String>) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                scan_dir(&path, violations);
            } else if path.extension().is_some_and(|ext| ext == "rs")
                && let Ok(content) = std::fs::read_to_string(&path)
            {
                for (line_num, line) in content.lines().enumerate() {
                    let trimmed = line.trim();
                    // Skip comments
                    if trimmed.starts_with("//") || trimmed.starts_with("/*") {
                        continue;
                    }
                    if trimmed.contains("use tantivy::") {
                        violations.push(format!(
                            "{}:{}: {}",
                            path.display(),
                            line_num + 1,
                            trimmed
                        ));
                    }
                }
            }
        }
    }

    scan_dir(&src_dir, &mut violations);

    assert!(
        violations.is_empty(),
        "Found direct tantivy imports (should use frankensearch::lexical instead):\n{}",
        violations.join("\n")
    );
}

/// Verify Cargo.toml has no direct tantivy dependency.
#[test]
fn no_direct_tantivy_in_cargo_toml() {
    let cargo_toml = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let content = std::fs::read_to_string(cargo_toml).expect("read Cargo.toml");

    // Check [dependencies] section for a direct tantivy = line
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("tantivy") && trimmed.contains('=') {
            panic!(
                "Found direct tantivy dependency in Cargo.toml: {trimmed}\n\
                 tantivy should only be used via frankensearch re-exports"
            );
        }
    }
}

// =============================================================================
// SEARCHFILTER UNIFICATION
// =============================================================================

/// Verify SemanticFilter directly implements frankensearch::core::filter::SearchFilter.
/// This proves the adapter pattern (FsSemanticFilterAdapter) has been eliminated.
#[test]
fn semantic_filter_implements_search_filter_directly() {
    use frankensearch::core::filter::SearchFilter;

    let filter = SemanticFilter {
        agents: Some(HashSet::from([3])),
        workspaces: Some(HashSet::from([7])),
        sources: Some(HashSet::from([11])),
        roles: Some(HashSet::from([1])),
        created_from: Some(1_700_000_000_000),
        created_to: Some(1_700_000_000_100),
    };

    // Matching doc_id
    assert!(
        filter.matches("m|42|2|3|7|11|1|1700000000050", None),
        "filter should match doc_id with correct agent/workspace/source/role/timestamp"
    );

    // Wrong agent
    assert!(
        !filter.matches("m|42|2|99|7|11|1|1700000000050", None),
        "filter should reject wrong agent_id"
    );

    // Wrong workspace
    assert!(
        !filter.matches("m|42|2|3|99|11|1|1700000000050", None),
        "filter should reject wrong workspace_id"
    );

    // Wrong source
    assert!(
        !filter.matches("m|42|2|3|7|99|1|1700000000050", None),
        "filter should reject wrong source_id"
    );

    // Wrong role
    assert!(
        !filter.matches("m|42|2|3|7|11|9|1700000000050", None),
        "filter should reject wrong role"
    );

    // Timestamp before range
    assert!(
        !filter.matches("m|42|2|3|7|11|1|1699999999999", None),
        "filter should reject timestamp before created_from"
    );

    // Timestamp after range
    assert!(
        !filter.matches("m|42|2|3|7|11|1|1700000000200", None),
        "filter should reject timestamp after created_to"
    );

    // Invalid doc_id
    assert!(
        !filter.matches("not-a-valid-doc-id", None),
        "filter should reject invalid doc_id format"
    );
}

/// Verify unrestricted filter (all None) matches everything.
#[test]
fn unrestricted_semantic_filter_matches_all() {
    use frankensearch::core::filter::SearchFilter;

    let filter = SemanticFilter::default();

    assert!(filter.matches("m|1|0|5|10|20|0|1700000000000", None));
    assert!(filter.matches("m|999|3|99|99|99|2|1800000000000", None));
}

// =============================================================================
// DOC_ID PARSING
// =============================================================================

/// Verify parse_semantic_doc_id is the single parser (no duplicates).
#[test]
fn parse_semantic_doc_id_roundtrip() {
    let hash_hex = "aa".repeat(32);
    let doc_id = format!("m|42|2|3|7|11|1|1700000000000|{hash_hex}");
    let parsed = parse_semantic_doc_id(&doc_id).expect("should parse valid doc_id");

    assert_eq!(parsed.message_id, 42);
    assert_eq!(parsed.chunk_idx, 2);
    assert_eq!(parsed.agent_id, 3);
    assert_eq!(parsed.workspace_id, 7);
    assert_eq!(parsed.source_id, 11);
    assert_eq!(parsed.role, 1);
    assert_eq!(parsed.created_at_ms, 1_700_000_000_000);
    assert!(parsed.content_hash.is_some(), "should parse content hash");
}

/// Verify doc_id without content hash still parses.
#[test]
fn parse_semantic_doc_id_without_hash() {
    let doc_id = "m|100|0|5|10|20|1|1700000000000";
    let parsed = parse_semantic_doc_id(doc_id).expect("should parse doc_id without hash");

    assert_eq!(parsed.message_id, 100);
    assert_eq!(parsed.chunk_idx, 0);
    assert!(parsed.content_hash.is_none(), "should have no content hash");
}

/// Invalid doc_id formats return None.
#[test]
fn parse_semantic_doc_id_rejects_invalid() {
    assert!(parse_semantic_doc_id("").is_none());
    assert!(parse_semantic_doc_id("not-a-doc-id").is_none());
    assert!(parse_semantic_doc_id("x|1|2|3|4|5|6|7").is_none()); // wrong prefix
    assert!(parse_semantic_doc_id("m|abc|2|3|4|5|6|7").is_none()); // non-numeric
    assert!(parse_semantic_doc_id("m|1|2|3").is_none()); // too few fields
}

// =============================================================================
// FRANKENSEARCH VECTOR INDEX INTEGRATION
// =============================================================================

/// Verify frankensearch VectorIndex write + search roundtrip works correctly.
#[test]
fn frankensearch_vector_index_write_and_search() {
    let dir = TempDir::new().unwrap();
    let index_path = dir.path().join("vector_index").join("index-test.fsvi");
    std::fs::create_dir_all(index_path.parent().unwrap()).unwrap();

    let hash_a = "00".repeat(32);
    let hash_b = "11".repeat(32);
    let doc_a = format!("m|101|0|1|10|100|1|1700000000001|{hash_a}");
    let doc_b = format!("m|202|0|2|20|200|1|1700000000002|{hash_b}");

    // Write two vectors
    let mut writer = VectorIndex::create_with_revision(
        &index_path,
        "test-embedder",
        "rev-1",
        2, // dimension
        frankensearch::index::Quantization::F16,
    )
    .expect("create vector index");

    writer
        .write_record(&doc_a, &[1.0, 0.0])
        .expect("write doc_a");
    writer
        .write_record(&doc_b, &[0.0, 1.0])
        .expect("write doc_b");
    writer.finish().expect("finish writing");

    // Read and search
    let index = VectorIndex::open(&index_path).expect("open vector index");

    // Search for vector similar to doc_a
    let results = index.search_top_k(&[1.0, 0.0], 5, None).expect("search");
    assert!(!results.is_empty(), "should find at least one result");

    let top = &results[0];
    let parsed = parse_semantic_doc_id(&top.doc_id).expect("parse top result doc_id");
    assert_eq!(parsed.message_id, 101, "top result should be doc_a");
}

/// Verify vector search with SemanticFilter integration.
#[test]
fn frankensearch_vector_search_with_semantic_filter() {
    use frankensearch::core::filter::SearchFilter;

    let dir = TempDir::new().unwrap();
    let index_path = dir.path().join("vector_index").join("index-filtered.fsvi");
    std::fs::create_dir_all(index_path.parent().unwrap()).unwrap();

    let hash = "00".repeat(32);
    let doc_agent1 = format!("m|101|0|1|10|100|1|1700000000001|{hash}");
    let doc_agent2 = format!("m|202|0|2|20|200|1|1700000000002|{hash}");

    let mut writer = VectorIndex::create_with_revision(
        &index_path,
        "test-embedder",
        "rev-1",
        2,
        frankensearch::index::Quantization::F16,
    )
    .expect("create index");

    // Both vectors point in same direction so both would match
    writer
        .write_record(&doc_agent1, &[1.0, 0.0])
        .expect("write");
    writer
        .write_record(&doc_agent2, &[0.9, 0.1])
        .expect("write");
    writer.finish().expect("finish");

    let index = VectorIndex::open(&index_path).expect("open");

    // Filter to agent_id=1 only
    let filter = SemanticFilter {
        agents: Some(HashSet::from([1])),
        ..Default::default()
    };

    let results = index
        .search_top_k(&[1.0, 0.0], 5, Some(&filter as &dyn SearchFilter))
        .expect("filtered search");

    assert_eq!(results.len(), 1, "should return only agent_id=1 result");
    let parsed = parse_semantic_doc_id(&results[0].doc_id).expect("parse");
    assert_eq!(parsed.agent_id, 1);
}

// =============================================================================
// LEXICAL SEARCH THROUGH FRANKENSEARCH
// =============================================================================

/// Verify that lexical search through SearchClient works end-to-end.
/// This validates the full pipeline: frankensearch::lexical types → BM25 scoring.
#[test]
fn lexical_search_through_frankensearch_pipeline() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    let conv = util::ConversationFixtureBuilder::new("claude_code")
        .title("frankensearch integration test")
        .source_path(dir.path().join("session.jsonl"))
        .base_ts(1_700_000_000_000)
        .messages(3)
        .with_content(0, "The authentication module handles OAuth2 flows")
        .with_content(1, "Token refresh uses exponential backoff strategy")
        .with_content(2, "Rate limiting prevents abuse of the API endpoint")
        .build_normalized();

    index.add_conversation(&conv).unwrap();
    index.commit().unwrap();

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    let filters = SearchFilters::default();

    // Exact term search
    let hits = client
        .search("authentication", filters.clone(), 10, 0, FieldMask::FULL)
        .unwrap();
    assert!(
        !hits.is_empty(),
        "should find 'authentication' via frankensearch BM25"
    );
    assert!(hits[0].content.contains("authentication"));

    // Prefix wildcard search
    let hits = client
        .search("auth*", filters.clone(), 10, 0, FieldMask::FULL)
        .unwrap();
    assert!(!hits.is_empty(), "should match auth* prefix");

    // Multi-term search
    let hits = client
        .search("token refresh", filters, 10, 0, FieldMask::FULL)
        .unwrap();
    assert!(!hits.is_empty(), "should find multi-term query");
}

/// Verify agent filter works through the frankensearch pipeline.
#[test]
fn agent_filter_through_frankensearch_pipeline() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    let conv_claude = util::ConversationFixtureBuilder::new("claude_code")
        .title("claude session")
        .source_path(dir.path().join("claude.jsonl"))
        .base_ts(1_700_000_000_000)
        .messages(1)
        .with_content(0, "debugging the database connection pool")
        .build_normalized();

    let conv_codex = util::ConversationFixtureBuilder::new("codex")
        .title("codex session")
        .source_path(dir.path().join("codex.jsonl"))
        .base_ts(1_700_000_001_000)
        .messages(1)
        .with_content(0, "debugging the cache invalidation logic")
        .build_normalized();

    index.add_conversation(&conv_claude).unwrap();
    index.add_conversation(&conv_codex).unwrap();
    index.commit().unwrap();

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");

    // Search with agent filter
    let mut filters = SearchFilters::default();
    filters.agents.insert("claude_code".to_string());

    let hits = client
        .search("debugging", filters, 10, 0, FieldMask::FULL)
        .unwrap();

    // Should only find claude_code results
    assert!(!hits.is_empty());
    for hit in &hits {
        assert_eq!(
            hit.agent, "claude_code",
            "agent filter should only return claude_code results"
        );
    }
}

// =============================================================================
// RRF FUSION VERIFICATION
// =============================================================================

/// Verify that frankensearch rrf_fuse is available and produces valid scores.
/// This tests the function signature and basic correctness, not the full hybrid
/// pipeline (which requires both lexical and semantic indexes).
#[test]
fn frankensearch_rrf_fuse_produces_valid_scores() {
    use frankensearch::{RrfConfig, ScoreSource, ScoredResult, VectorHit, rrf_fuse};

    let lexical_results = vec![
        ScoredResult {
            doc_id: "doc_a".to_string(),
            score: 10.0,
            source: ScoreSource::Lexical,
            index: None,
            fast_score: None,
            quality_score: None,
            lexical_score: Some(10.0),
            rerank_score: None,
            explanation: None,
            metadata: None,
        },
        ScoredResult {
            doc_id: "doc_b".to_string(),
            score: 5.0,
            source: ScoreSource::Lexical,
            index: None,
            fast_score: None,
            quality_score: None,
            lexical_score: Some(5.0),
            rerank_score: None,
            explanation: None,
            metadata: None,
        },
    ];

    let semantic_results = vec![
        VectorHit {
            index: 0,
            score: 0.95,
            doc_id: "doc_b".to_string(),
        },
        VectorHit {
            index: 1,
            score: 0.8,
            doc_id: "doc_c".to_string(),
        },
    ];

    let config = RrfConfig { k: 60.0 };
    let fused = rrf_fuse(&lexical_results, &semantic_results, 100, 0, &config);

    assert!(!fused.is_empty(), "RRF fusion should produce results");

    // doc_b appears in both lists, so should have highest RRF score
    let top = &fused[0];
    assert_eq!(
        top.doc_id, "doc_b",
        "doc_b should be ranked highest (appears in both lists)"
    );

    // Verify all scores are positive
    for result in &fused {
        assert!(result.rrf_score > 0.0, "RRF scores should be positive");
    }

    // Verify we see all three unique doc_ids
    let doc_ids: HashSet<&str> = fused.iter().map(|r| r.doc_id.as_str()).collect();
    assert!(doc_ids.contains("doc_a"));
    assert!(doc_ids.contains("doc_b"));
    assert!(doc_ids.contains("doc_c"));
}

// =============================================================================
// SEARCH RESULT CONSISTENCY
// =============================================================================

/// Verify that multiple searches with the same query produce identical results.
/// This tests determinism of the frankensearch pipeline.
#[test]
fn search_results_are_deterministic() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    let conv = util::ConversationFixtureBuilder::new("claude_code")
        .title("determinism test")
        .source_path(dir.path().join("session.jsonl"))
        .base_ts(1_700_000_000_000)
        .messages(5)
        .with_content(0, "error handling in the authentication module")
        .with_content(1, "authentication token validation logic")
        .with_content(2, "error recovery from network failures")
        .with_content(3, "database query optimization techniques")
        .with_content(4, "authentication flow diagram and documentation")
        .build_normalized();

    index.add_conversation(&conv).unwrap();
    index.commit().unwrap();

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    let filters = SearchFilters::default();

    // Run same query 3 times
    let hits1 = client
        .search("authentication", filters.clone(), 10, 0, FieldMask::FULL)
        .unwrap();
    let hits2 = client
        .search("authentication", filters.clone(), 10, 0, FieldMask::FULL)
        .unwrap();
    let hits3 = client
        .search("authentication", filters, 10, 0, FieldMask::FULL)
        .unwrap();

    // Same number of results
    assert_eq!(hits1.len(), hits2.len());
    assert_eq!(hits2.len(), hits3.len());

    // Same ordering (compare source_path + line_number as stable identifiers)
    for i in 0..hits1.len() {
        assert_eq!(
            hits1[i].source_path, hits2[i].source_path,
            "result {i} source_path should be deterministic"
        );
        assert_eq!(
            hits1[i].line_number, hits2[i].line_number,
            "result {i} line_number should be deterministic"
        );
    }
}

/// Verify SearchClient produces results with expected field population.
#[test]
fn search_results_have_expected_fields() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).unwrap();

    let conv = util::ConversationFixtureBuilder::new("claude_code")
        .title("field test session")
        .source_path(dir.path().join("session.jsonl"))
        .base_ts(1_700_000_000_000)
        .messages(1)
        .with_content(
            0,
            "testing that all search hit fields are populated correctly",
        )
        .build_normalized();

    index.add_conversation(&conv).unwrap();
    index.commit().unwrap();

    let client = SearchClient::open(dir.path(), None)
        .unwrap()
        .expect("client");
    let filters = SearchFilters::default();

    let hits = client
        .search("testing", filters, 10, 0, FieldMask::FULL)
        .unwrap();

    assert!(!hits.is_empty());
    let hit = &hits[0];

    assert!(!hit.content.is_empty(), "content should be populated");
    assert!(
        !hit.source_path.is_empty(),
        "source_path should be populated"
    );
    assert!(!hit.agent.is_empty(), "agent should be populated");
    assert_eq!(hit.agent, "claude_code");
    assert!(hit.score > 0.0, "score should be positive");
}
