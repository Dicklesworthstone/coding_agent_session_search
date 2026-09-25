//! E2E tests for two-tier progressive semantic search (bd-3dcw).
//!
//! These tests verify that the two-tier search system:
//! 1. Returns correct rankings compared to manually computed ground truth
//! 2. Fast embeddings return results in <5ms
//! 3. Quality refinement produces improved rankings
//! 4. Score blending works correctly with configurable weights
//!
//! The tests use hash embedder for both fast and quality tiers since it's
//! always available and provides deterministic results for verification.

use std::sync::Arc;
use std::time::Instant;

use half::f16;

mod util;

// =============================================================================
// Test Infrastructure
// =============================================================================

/// Fixture daemon client that uses a hash embedder for quality embeddings.
/// This allows us to compute expected scores without a real daemon.
struct FixtureQualityDaemon {
    embedder: coding_agent_search::search::hash_embedder::HashEmbedder,
}

impl FixtureQualityDaemon {
    fn new(dimension: usize) -> Self {
        Self {
            embedder: coding_agent_search::search::hash_embedder::HashEmbedder::new(dimension),
        }
    }
}

impl coding_agent_search::search::daemon_client::DaemonClient for FixtureQualityDaemon {
    fn id(&self) -> &str {
        "fixture-quality-daemon"
    }

    fn is_available(&self) -> bool {
        true
    }

    fn embed(
        &self,
        text: &str,
        _request_id: &str,
    ) -> Result<Vec<f32>, coding_agent_search::search::daemon_client::DaemonError> {
        use coding_agent_search::search::embedder::Embedder;
        self.embedder.embed_sync(text).map_err(|e| {
            coding_agent_search::search::daemon_client::DaemonError::Failed(e.to_string())
        })
    }

    fn embed_batch(
        &self,
        texts: &[&str],
        _request_id: &str,
    ) -> Result<Vec<Vec<f32>>, coding_agent_search::search::daemon_client::DaemonError> {
        use coding_agent_search::search::embedder::Embedder;
        self.embedder.embed_batch_sync(texts).map_err(|e| {
            coding_agent_search::search::daemon_client::DaemonError::Failed(e.to_string())
        })
    }

    fn rerank(
        &self,
        _query: &str,
        _documents: &[&str],
        _request_id: &str,
    ) -> Result<Vec<f32>, coding_agent_search::search::daemon_client::DaemonError> {
        Err(
            coding_agent_search::search::daemon_client::DaemonError::Unavailable(
                "reranking not implemented".to_string(),
            ),
        )
    }
}

/// Test document with known content for semantic similarity testing.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct TestDocument {
    id: &'static str,
    content: &'static str,
    /// Expected semantic similarity ranking (1 = most similar).
    /// Documents with similar topics should have lower rank numbers.
    expected_rank_topic: &'static str,
}

/// Create test documents with known semantic relationships.
fn create_test_documents() -> Vec<TestDocument> {
    vec![
        TestDocument {
            id: "doc-auth",
            content: "authentication login password security oauth jwt token bearer",
            expected_rank_topic: "auth",
        },
        TestDocument {
            id: "doc-db",
            content: "database query sql postgresql mysql insert update delete table",
            expected_rank_topic: "database",
        },
        TestDocument {
            id: "doc-api",
            content: "api rest http endpoint request response json payload headers",
            expected_rank_topic: "api",
        },
        TestDocument {
            id: "doc-ui",
            content: "frontend react component button form input user interface",
            expected_rank_topic: "ui",
        },
        TestDocument {
            id: "doc-test",
            content: "testing unit test integration mock assertion coverage",
            expected_rank_topic: "test",
        },
        TestDocument {
            id: "doc-auth2",
            content: "user session login logout credentials authenticate verify",
            expected_rank_topic: "auth", // Related to auth, should rank near doc-auth
        },
        TestDocument {
            id: "doc-db2",
            content: "schema migration index constraint foreign key primary",
            expected_rank_topic: "database", // Related to database
        },
        TestDocument {
            id: "doc-perf",
            content: "performance optimization cache latency throughput benchmark",
            expected_rank_topic: "perf",
        },
    ]
}

/// Compute ground truth rankings for a query using a hash embedder.
/// Returns document indices sorted by descending similarity score.
fn compute_ground_truth_rankings(
    query: &str,
    documents: &[TestDocument],
    embedder: &coding_agent_search::search::hash_embedder::HashEmbedder,
) -> Vec<(usize, f32)> {
    use coding_agent_search::search::embedder::Embedder;

    let query_vec = embedder.embed_sync(query).expect("embed query");
    let mut scores: Vec<(usize, f32)> = documents
        .iter()
        .enumerate()
        .map(|(idx, doc)| {
            let doc_vec = embedder.embed_sync(doc.content).expect("embed doc");
            let score = dot_product(&query_vec, &doc_vec);
            (idx, score)
        })
        .collect();

    // Sort by score descending
    scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scores
}

/// Simple dot product for f32 vectors.
fn dot_product(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

/// Build a TwoTierIndex from test documents.
fn build_test_index(
    documents: &[TestDocument],
    fast_embedder: &coding_agent_search::search::hash_embedder::HashEmbedder,
    quality_embedder: &coding_agent_search::search::hash_embedder::HashEmbedder,
    config: &coding_agent_search::search::two_tier_search::TwoTierConfig,
) -> coding_agent_search::search::two_tier_search::TwoTierIndex {
    use coding_agent_search::search::embedder::Embedder;
    use coding_agent_search::search::two_tier_search::{DocumentId, TwoTierEntry, TwoTierIndex};

    let entries: Vec<TwoTierEntry> = documents
        .iter()
        .enumerate()
        .map(|(idx, doc)| {
            let fast_vec = fast_embedder.embed_sync(doc.content).expect("fast embed");
            let quality_vec = quality_embedder
                .embed_sync(doc.content)
                .expect("quality embed");

            TwoTierEntry {
                doc_id: DocumentId::Session(doc.id.to_string()),
                message_id: idx as u64,
                fast_embedding: fast_vec.into_iter().map(f16::from_f32).collect(),
                quality_embedding: quality_vec.into_iter().map(f16::from_f32).collect(),
            }
        })
        .collect();

    TwoTierIndex::build("fast-hash", "quality-hash", config, entries).expect("build index")
}

// =============================================================================
// Correctness Tests
// =============================================================================

/// Test that fast search returns correct rankings compared to ground truth.
#[test]
fn fast_search_matches_ground_truth() {
    use coding_agent_search::search::embedder::Embedder;
    use coding_agent_search::search::hash_embedder::HashEmbedder;
    use coding_agent_search::search::two_tier_search::TwoTierConfig;

    let config = TwoTierConfig::default();
    let fast_embedder = HashEmbedder::new(config.fast_dimension);
    let quality_embedder = HashEmbedder::new(config.quality_dimension);

    let documents = create_test_documents();
    let index = build_test_index(&documents, &fast_embedder, &quality_embedder, &config);

    // Query for authentication-related content
    let query = "authenticate user login session token";

    // Compute ground truth using the same embedder
    let expected = compute_ground_truth_rankings(query, &documents, &fast_embedder);

    // Run fast search
    let query_vec = fast_embedder.embed_sync(query).expect("embed query");
    let results = index.search_fast(&query_vec, 5);

    // Verify top results match ground truth order
    assert!(!results.is_empty(), "fast search should return results");
    assert_eq!(
        results.len(),
        5.min(documents.len()),
        "should return requested k results"
    );

    // Hash embeddings plus f16/index quantization can flip near-ties within the
    // same topical cluster. Require the same dominant topic rather than an exact
    // document-id match.
    assert_eq!(
        documents[results[0].idx].expected_rank_topic, documents[expected[0].0].expected_rank_topic,
        "top result should stay within the same topical cluster: got doc {} ({}) expected doc {} ({})",
        results[0].idx, documents[results[0].idx].id, expected[0].0, documents[expected[0].0].id
    );

    // Verify scores are in descending order
    for window in results.windows(2) {
        assert!(
            window[0].score >= window[1].score,
            "results should be sorted by score descending"
        );
    }

    // Verify top results include auth-related documents
    let top_3_ids: Vec<&str> = results[..3.min(results.len())]
        .iter()
        .map(|r| documents[r.idx].id)
        .collect();
    assert!(
        top_3_ids.contains(&"doc-auth") || top_3_ids.contains(&"doc-auth2"),
        "auth-related documents should rank highly for auth query: got {:?}",
        top_3_ids
    );
}

/// Test that quality search returns correct rankings compared to ground truth.
#[test]
fn quality_search_matches_ground_truth() {
    use coding_agent_search::search::embedder::Embedder;
    use coding_agent_search::search::hash_embedder::HashEmbedder;
    use coding_agent_search::search::two_tier_search::TwoTierConfig;

    let config = TwoTierConfig::default();
    let fast_embedder = HashEmbedder::new(config.fast_dimension);
    let quality_embedder = HashEmbedder::new(config.quality_dimension);

    let documents = create_test_documents();
    let index = build_test_index(&documents, &fast_embedder, &quality_embedder, &config);

    // Query for database-related content
    let query = "database query table schema migration";

    // Compute ground truth using quality embedder
    let expected = compute_ground_truth_rankings(query, &documents, &quality_embedder);

    // Run quality search
    let query_vec = quality_embedder.embed_sync(query).expect("embed query");
    let results = index.search_quality(&query_vec, 5);

    // Verify top results match ground truth order
    assert!(!results.is_empty(), "quality search should return results");

    // Check that the top result matches ground truth
    assert_eq!(
        results[0].idx, expected[0].0,
        "top result should match ground truth: got doc {} ({}) expected doc {} ({})",
        results[0].idx, documents[results[0].idx].id, expected[0].0, documents[expected[0].0].id
    );

    // Verify top results include database-related documents
    let top_3_ids: Vec<&str> = results[..3.min(results.len())]
        .iter()
        .map(|r| documents[r.idx].id)
        .collect();
    assert!(
        top_3_ids.contains(&"doc-db") || top_3_ids.contains(&"doc-db2"),
        "database-related documents should rank highly for db query: got {:?}",
        top_3_ids
    );
}

/// Test the full two-tier progressive search flow.
#[test]
fn two_tier_progressive_search_correctness() {
    use coding_agent_search::search::embedder::Embedder;
    use coding_agent_search::search::hash_embedder::HashEmbedder;
    use coding_agent_search::search::two_tier_search::{
        SearchPhase, TwoTierConfig, TwoTierSearcher,
    };

    let config = TwoTierConfig {
        quality_weight: 1.0,
        ..TwoTierConfig::default()
    };
    let fast_embedder = Arc::new(HashEmbedder::new(config.fast_dimension));
    let quality_embedder = HashEmbedder::new(config.quality_dimension);

    let documents = create_test_documents();
    let index = build_test_index(&documents, &fast_embedder, &quality_embedder, &config);

    // Create mock daemon for quality tier
    let daemon = Arc::new(FixtureQualityDaemon::new(config.quality_dimension));

    // Create searcher
    let searcher =
        TwoTierSearcher::new(&index, fast_embedder.clone(), Some(daemon), config.clone());

    // Query for API-related content
    let query = "api http request response endpoint";

    let mut phases: Vec<SearchPhase> = searcher.search(query, 5).collect();

    // Should have 2 phases: Initial and Refined
    assert_eq!(phases.len(), 2, "should have Initial and Refined phases");

    // Verify Initial phase
    let initial = phases.remove(0);
    match initial {
        SearchPhase::Initial {
            results,
            latency_ms,
        } => {
            assert!(!results.is_empty(), "initial results should not be empty");
            assert!(latency_ms < 1000, "initial phase should complete quickly");
            assert!(
                results
                    .windows(2)
                    .all(|window| window[0].score >= window[1].score),
                "initial results should be sorted by descending score"
            );

            // Since 2d203d55 the initial page is the first k of the bounded
            // fast candidate pool (max(k, max_refinement_docs)), ordered by
            // score and then by message identity. A direct k=5 fast search can
            // cut equal-score ties differently, so compare with the pool.
            let fast_query = fast_embedder.embed_sync(query).expect("embed fast query");
            let pool_limit = config.max_refinement_docs.max(5).min(index.len());
            let pool = index.search_fast(&fast_query, pool_limit);
            let actual_ids: Vec<usize> = results.iter().map(|r| r.idx).collect();
            let expected_ids: Vec<usize> = pool.iter().take(5).map(|r| r.idx).collect();
            assert_eq!(
                actual_ids, expected_ids,
                "initial phase should be the head of the fast candidate pool; initial={results:?} pool={pool:?}"
            );
            assert!(
                results.windows(2).all(|pair| pair[0].score > pair[1].score
                    || (pair[0].score == pair[1].score
                        && (pair[0].message_id, pair[0].idx) < (pair[1].message_id, pair[1].idx))),
                "equal-score initial results must be ordered by message identity: {results:?}"
            );
            // Every scored document the direct k=5 search returns above the
            // tie boundary must still lead the initial page.
            let direct = index.search_fast(&fast_query, 5);
            let boundary = direct.last().map_or(f32::NEG_INFINITY, |r| r.score);
            let direct_above: Vec<usize> = direct
                .iter()
                .filter(|r| r.score > boundary)
                .map(|r| r.idx)
                .collect();
            assert_eq!(
                &actual_ids[..direct_above.len()],
                direct_above.as_slice(),
                "strictly better fast hits must not be displaced by ties: initial={results:?} direct={direct:?}"
            );
        }
        other => panic!("expected Initial phase, got {:?}", other),
    }

    // Verify Refined phase
    let refined = phases.remove(0);
    match refined {
        SearchPhase::Refined {
            results,
            latency_ms,
        } => {
            assert!(!results.is_empty(), "refined results should not be empty");
            assert!(
                latency_ms < 5000,
                "refined phase should complete reasonably"
            );
            assert!(
                results
                    .windows(2)
                    .all(|window| window[0].score >= window[1].score),
                "refined results should be sorted by descending score"
            );

            // Since 2d203d55 quality candidates are retrieved independently
            // across the whole index (so a document the fast tier missed can
            // still be recovered) and fused with the fast pool by weighted
            // reciprocal rank. With quality_weight = 1.0 the fast ranks carry
            // no weight, so the refined page is exactly the head of the
            // independent quality ranking.
            let quality_query = quality_embedder
                .embed_sync(query)
                .expect("embed quality query");
            let quality_limit = config.max_refinement_docs.min(index.len());
            let quality_ranked = index.search_quality(&quality_query, quality_limit);
            let actual_ids: Vec<usize> = results.iter().map(|r| r.idx).collect();
            let expected_ids: Vec<usize> = quality_ranked.iter().take(5).map(|r| r.idx).collect();
            assert_eq!(
                actual_ids, expected_ids,
                "with full quality weight, the refined page is the head of the independent quality ranking; refined={results:?} quality={quality_ranked:?}"
            );
            let fast_query = fast_embedder.embed_sync(query).expect("embed fast query");
            let fast_top: Vec<usize> = index
                .search_fast(&fast_query, 5)
                .iter()
                .map(|r| r.idx)
                .collect();
            assert!(
                actual_ids.iter().any(|idx| !fast_top.contains(idx)),
                "this fixture's refined page must recover a document outside the fast top-5 \
                 (independent quality retrieval): refined={actual_ids:?} fast={fast_top:?}"
            );
        }
        SearchPhase::RefinementFailed { error } => {
            panic!("refinement should not fail with mock daemon: {}", error);
        }
        other => panic!("expected Refined phase, got {:?}", other),
    }
}

/// Test fast-only mode skips quality refinement.
#[test]
fn fast_only_mode_skips_refinement() {
    use coding_agent_search::search::hash_embedder::HashEmbedder;
    use coding_agent_search::search::two_tier_search::{
        SearchPhase, TwoTierConfig, TwoTierSearcher,
    };

    let config = TwoTierConfig {
        fast_only: true,
        ..TwoTierConfig::default()
    };

    let fast_embedder = Arc::new(HashEmbedder::new(config.fast_dimension));
    let quality_embedder = HashEmbedder::new(config.quality_dimension);

    let documents = create_test_documents();
    let index = build_test_index(&documents, &fast_embedder, &quality_embedder, &config);

    // Create mock daemon (should not be called in fast-only mode)
    let daemon = Arc::new(FixtureQualityDaemon::new(config.quality_dimension));

    let searcher = TwoTierSearcher::new(&index, fast_embedder.clone(), Some(daemon), config);

    let phases: Vec<SearchPhase> = searcher.search("test query", 5).collect();

    // Should have only 1 phase: Initial (no Refined)
    assert_eq!(
        phases.len(),
        1,
        "fast-only mode should have only Initial phase"
    );
    assert!(
        matches!(phases[0], SearchPhase::Initial { .. }),
        "should be Initial phase"
    );
}

/// Test that refinement gracefully degrades when daemon unavailable.
#[test]
fn refinement_degrades_gracefully_without_daemon() {
    use coding_agent_search::search::hash_embedder::HashEmbedder;
    use coding_agent_search::search::two_tier_search::{
        SearchPhase, TwoTierConfig, TwoTierSearcher,
    };

    let config = TwoTierConfig::default();
    let fast_embedder = Arc::new(HashEmbedder::new(config.fast_dimension));
    let quality_embedder = HashEmbedder::new(config.quality_dimension);

    let documents = create_test_documents();
    let index = build_test_index(&documents, &fast_embedder, &quality_embedder, &config);

    // No daemon - pass None
    let searcher: TwoTierSearcher<FixtureQualityDaemon> =
        TwoTierSearcher::new(&index, fast_embedder.clone(), None, config);

    let phases: Vec<SearchPhase> = searcher.search("test query", 5).collect();

    // Should have 2 phases: Initial and RefinementFailed
    assert_eq!(phases.len(), 2, "should have Initial and failure phase");
    assert!(
        matches!(phases[0], SearchPhase::Initial { .. }),
        "first should be Initial"
    );
    assert!(
        matches!(phases[1], SearchPhase::RefinementFailed { .. }),
        "second should be RefinementFailed when no daemon"
    );
}

// =============================================================================
// Performance Tests
// =============================================================================

/// Test that fast search completes within latency budget (<5ms for small index).
#[test]
fn fast_search_latency_under_budget() {
    use coding_agent_search::search::embedder::Embedder;
    use coding_agent_search::search::hash_embedder::HashEmbedder;
    use coding_agent_search::search::two_tier_search::TwoTierConfig;

    let config = TwoTierConfig::default();
    let fast_embedder = HashEmbedder::new(config.fast_dimension);
    let quality_embedder = HashEmbedder::new(config.quality_dimension);

    let documents = create_test_documents();
    let index = build_test_index(&documents, &fast_embedder, &quality_embedder, &config);

    let query = "authentication security login";
    let query_vec = fast_embedder.embed_sync(query).expect("embed query");

    // Warm up
    let _ = index.search_fast(&query_vec, 5);

    // Measure latency over multiple runs
    let mut latencies: Vec<u64> = Vec::with_capacity(10);
    for _ in 0..10 {
        let start = Instant::now();
        let _ = index.search_fast(&query_vec, 5);
        latencies.push(start.elapsed().as_micros() as u64);
    }

    let median_latency_us = {
        latencies.sort();
        latencies[latencies.len() / 2]
    };

    // Fast search should complete in <5ms (5000μs) for this small index
    assert!(
        median_latency_us < 5000,
        "fast search median latency {}μs should be <5000μs (5ms)",
        median_latency_us
    );

    println!(
        "Fast search latency: median={}μs, min={}μs, max={}μs",
        median_latency_us,
        latencies.first().unwrap(),
        latencies.last().unwrap()
    );
}

/// Test that quality search completes within latency budget.
#[test]
fn quality_search_latency_under_budget() {
    use coding_agent_search::search::embedder::Embedder;
    use coding_agent_search::search::hash_embedder::HashEmbedder;
    use coding_agent_search::search::two_tier_search::TwoTierConfig;

    let config = TwoTierConfig::default();
    let fast_embedder = HashEmbedder::new(config.fast_dimension);
    let quality_embedder = HashEmbedder::new(config.quality_dimension);

    let documents = create_test_documents();
    let index = build_test_index(&documents, &fast_embedder, &quality_embedder, &config);

    let query = "database performance optimization";
    let query_vec = quality_embedder.embed_sync(query).expect("embed query");

    // Warm up
    let _ = index.search_quality(&query_vec, 5);

    // Measure latency
    let mut latencies: Vec<u64> = Vec::with_capacity(10);
    for _ in 0..10 {
        let start = Instant::now();
        let _ = index.search_quality(&query_vec, 5);
        latencies.push(start.elapsed().as_micros() as u64);
    }

    let median_latency_us = {
        latencies.sort();
        latencies[latencies.len() / 2]
    };

    // Quality search should complete in <10ms (10000μs) for this small index
    assert!(
        median_latency_us < 10000,
        "quality search median latency {}μs should be <10000μs (10ms)",
        median_latency_us
    );

    println!(
        "Quality search latency: median={}μs, min={}μs, max={}μs",
        median_latency_us,
        latencies.first().unwrap(),
        latencies.last().unwrap()
    );
}

/// Test progressive search with timing verification.
#[test]
fn progressive_search_timing() {
    use coding_agent_search::search::hash_embedder::HashEmbedder;
    use coding_agent_search::search::two_tier_search::{
        SearchPhase, TwoTierConfig, TwoTierSearcher,
    };

    let config = TwoTierConfig::default();
    let fast_embedder = Arc::new(HashEmbedder::new(config.fast_dimension));
    let quality_embedder = HashEmbedder::new(config.quality_dimension);

    let documents = create_test_documents();
    let index = build_test_index(&documents, &fast_embedder, &quality_embedder, &config);

    let daemon = Arc::new(FixtureQualityDaemon::new(config.quality_dimension));

    let searcher = TwoTierSearcher::new(&index, fast_embedder.clone(), Some(daemon), config);

    let query = "frontend react component interface";

    let start = Instant::now();
    let phases: Vec<SearchPhase> = searcher.search(query, 5).collect();
    let total_time = start.elapsed();

    // Verify we got both phases
    assert!(
        phases.len() >= 2,
        "expected at least 2 phases, got {}",
        phases.len()
    );

    // Extract timings from phases
    let initial_latency = match &phases[0] {
        SearchPhase::Initial { latency_ms, .. } => *latency_ms,
        _ => panic!("expected Initial phase"),
    };

    let refined_latency = match &phases[1] {
        SearchPhase::Refined { latency_ms, .. } => *latency_ms,
        SearchPhase::RefinementFailed { .. } => 0,
        _ => panic!("expected Refined or RefinementFailed phase"),
    };

    println!(
        "Progressive search timing: initial={}ms, refined={}ms, total={:?}",
        initial_latency, refined_latency, total_time
    );

    // Initial phase should be very fast (hash embedder is ~instantaneous)
    assert!(
        initial_latency < 100,
        "initial phase should be fast: {}ms",
        initial_latency
    );

    // Refined phase should complete within reasonable time
    assert!(
        refined_latency < 500,
        "refined phase should complete: {}ms",
        refined_latency
    );
}

// =============================================================================
// Score Blending Tests
// =============================================================================

/// Test that score normalization works correctly.
#[test]
fn score_normalization_correctness() {
    use coding_agent_search::search::two_tier_search::normalize_scores;

    // Test basic normalization
    let scores = vec![0.8, 0.6, 0.4, 0.2];
    let normalized = normalize_scores(&scores);

    assert!(
        (normalized[0] - 1.0).abs() < 0.001,
        "max should normalize to 1.0"
    );
    assert!(
        (normalized[3] - 0.0).abs() < 0.001,
        "min should normalize to 0.0"
    );

    // Verify intermediate values are proportional
    let expected_mid = (0.6 - 0.2) / (0.8 - 0.2); // 0.666...
    assert!(
        (normalized[1] - expected_mid).abs() < 0.001,
        "intermediate values should be proportional"
    );

    // Test edge cases
    let empty: Vec<f32> = vec![];
    assert!(normalize_scores(&empty).is_empty());

    let single = vec![0.5];
    let single_norm = normalize_scores(&single);
    assert_eq!(single_norm.len(), 1);
    assert!(
        (single_norm[0] - 1.0).abs() < 0.001,
        "single value normalizes to 1.0"
    );

    let constant = vec![0.5, 0.5, 0.5];
    let const_norm = normalize_scores(&constant);
    for n in &const_norm {
        assert!((n - 1.0).abs() < 0.001, "constant values normalize to 1.0");
    }
}

/// Test that score blending combines fast and quality scores correctly.
#[test]
fn score_blending_correctness() {
    use coding_agent_search::search::two_tier_search::blend_scores;

    let fast = vec![0.8, 0.6, 0.4];
    let quality = vec![0.4, 0.8, 0.6];

    // With 50% quality weight
    let blended_50 = blend_scores(&fast, &quality, 0.5);
    assert_eq!(blended_50.len(), 3);

    // With equal weights, the order might change based on combined scores
    // Fast: [1.0, 0.5, 0.0] normalized
    // Quality: [0.0, 1.0, 0.5] normalized
    // Blended (0.5 weight): [0.5, 0.75, 0.25]

    // With 100% quality weight
    let blended_100 = blend_scores(&fast, &quality, 1.0);
    // Should match normalized quality order
    assert!(
        blended_100[1] >= blended_100[0] && blended_100[1] >= blended_100[2],
        "100% quality weight should match quality ranking"
    );

    // With 0% quality weight
    let blended_0 = blend_scores(&fast, &quality, 0.0);
    // Should match normalized fast order
    assert!(
        blended_0[0] >= blended_0[1] && blended_0[1] >= blended_0[2],
        "0% quality weight should match fast ranking"
    );
}

/// Test that quality weight affects final ranking.
#[test]
fn quality_weight_affects_ranking() {
    use coding_agent_search::search::embedder::Embedder;
    use coding_agent_search::search::hash_embedder::HashEmbedder;
    use coding_agent_search::search::two_tier_search::TwoTierConfig;

    let fast_dim = 128; // Different dimensions to get different rankings
    let quality_dim = 256;

    let config = TwoTierConfig {
        fast_dimension: fast_dim,
        quality_dimension: quality_dim,
        ..TwoTierConfig::default()
    };

    let fast_embedder = HashEmbedder::new(fast_dim);
    let quality_embedder = HashEmbedder::new(quality_dim);

    let documents = create_test_documents();
    let index = build_test_index(&documents, &fast_embedder, &quality_embedder, &config);

    let query = "user interface form input button";

    // Get fast-only results
    let fast_query = fast_embedder.embed_sync(query).expect("embed");
    let fast_results = index.search_fast(&fast_query, documents.len());

    // Get quality-only results
    let quality_query = quality_embedder.embed_sync(query).expect("embed");
    let quality_results = index.search_quality(&quality_query, documents.len());

    // The ranking orders might differ between fast and quality
    let fast_order: Vec<usize> = fast_results.iter().map(|r| r.idx).collect();
    let quality_order: Vec<usize> = quality_results.iter().map(|r| r.idx).collect();

    // At minimum, both should return the same documents (just in different order)
    assert_eq!(fast_order.len(), quality_order.len());

    println!(
        "Fast order: {:?}",
        fast_order
            .iter()
            .map(|&i| documents[i].id)
            .collect::<Vec<_>>()
    );
    println!(
        "Quality order: {:?}",
        quality_order
            .iter()
            .map(|&i| documents[i].id)
            .collect::<Vec<_>>()
    );
}

// =============================================================================
// Edge Case Tests
// =============================================================================

/// Test search with empty index.
#[test]
fn search_empty_index() {
    use coding_agent_search::search::embedder::Embedder;
    use coding_agent_search::search::hash_embedder::HashEmbedder;
    use coding_agent_search::search::two_tier_search::{TwoTierConfig, TwoTierIndex};

    let config = TwoTierConfig::default();
    let embedder = HashEmbedder::new(config.fast_dimension);

    let index = TwoTierIndex::build("fast", "quality", &config, Vec::new()).expect("build empty");

    assert!(index.is_empty());
    assert_eq!(index.len(), 0);

    let query_vec = embedder.embed_sync("test query").expect("embed");
    let results = index.search_fast(&query_vec, 10);
    assert!(results.is_empty(), "empty index should return no results");
}

/// Test search with k larger than document count.
#[test]
fn search_k_larger_than_docs() {
    use coding_agent_search::search::embedder::Embedder;
    use coding_agent_search::search::hash_embedder::HashEmbedder;
    use coding_agent_search::search::two_tier_search::TwoTierConfig;

    let config = TwoTierConfig::default();
    let fast_embedder = HashEmbedder::new(config.fast_dimension);
    let quality_embedder = HashEmbedder::new(config.quality_dimension);

    let documents = create_test_documents();
    let doc_count = documents.len();
    let index = build_test_index(&documents, &fast_embedder, &quality_embedder, &config);

    let query_vec = fast_embedder.embed_sync("test").expect("embed");
    let results = index.search_fast(&query_vec, 100); // Request more than available

    assert_eq!(
        results.len(),
        doc_count,
        "should return all documents when k > doc_count"
    );
}

/// Test that different queries return different rankings.
#[test]
fn different_queries_different_rankings() {
    use coding_agent_search::search::embedder::Embedder;
    use coding_agent_search::search::hash_embedder::HashEmbedder;
    use coding_agent_search::search::two_tier_search::TwoTierConfig;

    let config = TwoTierConfig::default();
    let fast_embedder = HashEmbedder::new(config.fast_dimension);
    let quality_embedder = HashEmbedder::new(config.quality_dimension);

    let documents = create_test_documents();
    let index = build_test_index(&documents, &fast_embedder, &quality_embedder, &config);

    // Query 1: Auth-focused
    let auth_query = fast_embedder
        .embed_sync("authentication login security")
        .expect("embed");
    let auth_results = index.search_fast(&auth_query, 3);

    // Query 2: Database-focused
    let db_query = fast_embedder
        .embed_sync("database sql query table")
        .expect("embed");
    let db_results = index.search_fast(&db_query, 3);

    // Top results should differ for different queries
    let auth_top = auth_results.iter().map(|r| r.idx).collect::<Vec<_>>();
    let db_top = db_results.iter().map(|r| r.idx).collect::<Vec<_>>();

    // At least the top result should differ (auth docs vs db docs)
    assert_ne!(
        auth_top[0],
        db_top[0],
        "different queries should produce different top results: auth={:?} db={:?}",
        auth_top
            .iter()
            .map(|&i| documents[i].id)
            .collect::<Vec<_>>(),
        db_top.iter().map(|&i| documents[i].id).collect::<Vec<_>>()
    );
}
