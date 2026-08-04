//! Integration tests for the bakeoff evaluation harness.
//!
//! These tests verify the harness works correctly with fixture embedders.

use coding_agent_search::bakeoff::{
    EvaluationConfig, EvaluationCorpus, EvaluationHarness, ModelMetadata, format_comparison_table,
};
use coding_agent_search::search::embedder::{Embedder, EmbedderError, EmbedderResult};
use frankensearch::ModelCategory;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A fixture embedder for testing the harness.
/// Returns deterministic embeddings based on text hash.
struct FixtureEmbedder {
    id: String,
    dimension: usize,
    call_count: AtomicUsize,
}

impl FixtureEmbedder {
    fn new(id: &str, dimension: usize) -> Self {
        Self {
            id: id.to_string(),
            dimension,
            call_count: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.call_count.load(Ordering::SeqCst)
    }
}

impl Embedder for FixtureEmbedder {
    fn embed_sync(&self, text: &str) -> EmbedderResult<Vec<f32>> {
        if text.is_empty() {
            return Err(EmbedderError::InvalidConfig {
                field: "input_text".into(),
                value: "(empty)".into(),
                reason: "empty text".into(),
            });
        }
        self.call_count.fetch_add(1, Ordering::SeqCst);

        // Create deterministic embedding based on text
        let mut embedding = vec![0.0f32; self.dimension];
        let hash = text
            .bytes()
            .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));

        // Fill with pseudo-random but deterministic values
        for (i, v) in embedding.iter_mut().enumerate() {
            let seed = hash.wrapping_add(i as u64);
            *v = ((seed % 1000) as f32 / 1000.0) - 0.5;
        }

        // Normalize to unit length
        let norm: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for v in &mut embedding {
                *v /= norm;
            }
        }

        Ok(embedding)
    }

    fn dimension(&self) -> usize {
        self.dimension
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn is_semantic(&self) -> bool {
        true
    }

    fn category(&self) -> ModelCategory {
        ModelCategory::StaticEmbedder
    }
}

/// A fixture embedder that produces quality-aware embeddings.
/// Similar texts produce similar embeddings.
struct QualityFixtureEmbedder {
    id: String,
    dimension: usize,
}

impl QualityFixtureEmbedder {
    fn new(id: &str, dimension: usize) -> Self {
        Self {
            id: id.to_string(),
            dimension,
        }
    }

    /// Extract simple features from text for embedding.
    fn text_features(&self, text: &str) -> Vec<f32> {
        let text_lower = text.to_lowercase();
        let words: Vec<&str> = text_lower.split_whitespace().collect();

        let mut features = vec![0.0f32; self.dimension];

        // Feature 1: word count normalized
        features[0] = (words.len() as f32 / 20.0).min(1.0);

        // Feature 2-10: keyword presence
        let keywords = [
            "authentication",
            "jwt",
            "database",
            "error",
            "async",
            "json",
            "logging",
            "cli",
            "http",
            "test",
        ];
        for (i, keyword) in keywords.iter().enumerate() {
            if i + 1 < self.dimension && text_lower.contains(keyword) {
                features[i + 1] = 1.0;
            }
        }

        // Fill rest with hash-based values
        let hash = text
            .bytes()
            .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
        for (i, feature) in features.iter_mut().enumerate().skip(11) {
            let seed = hash.wrapping_add(i as u64);
            *feature = ((seed % 1000) as f32 / 1000.0) - 0.5;
        }

        // Normalize
        let norm: f32 = features.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for v in &mut features {
                *v /= norm;
            }
        }

        features
    }
}

impl Embedder for QualityFixtureEmbedder {
    fn embed_sync(&self, text: &str) -> EmbedderResult<Vec<f32>> {
        if text.is_empty() {
            return Err(EmbedderError::InvalidConfig {
                field: "input_text".into(),
                value: "(empty)".into(),
                reason: "empty text".into(),
            });
        }
        Ok(self.text_features(text))
    }

    fn dimension(&self) -> usize {
        self.dimension
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn is_semantic(&self) -> bool {
        true
    }

    fn category(&self) -> ModelCategory {
        ModelCategory::StaticEmbedder
    }
}

#[test]
fn test_harness_with_fixture_embedder() {
    let harness = EvaluationHarness::new();
    let corpus = EvaluationCorpus::code_search_sample();
    let embedder = FixtureEmbedder::new("fixture-384", 384);

    let metadata = ModelMetadata {
        id: "fixture-384".to_string(),
        name: "Fixture Embedder".to_string(),
        source: "test".to_string(),
        release_date: "2025-12-01".to_string(),
        dimension: Some(384),
        size_bytes: Some(50_000_000), // 50MB
        is_baseline: false,
    };

    let report = harness
        .evaluate(&embedder, &corpus, &metadata)
        .expect("evaluation should succeed");

    // Verify report structure
    assert_eq!(report.model_id, "fixture-384");
    assert!(!report.corpus_hash.is_empty());
    assert!(report.ndcg_at_10 >= 0.0 && report.ndcg_at_10 <= 1.0);
    // cold_start_ms can be 0 for very fast fixture embedders (sub-millisecond)
    assert!(report.eligible); // Released 2025-12-01, after cutoff

    // Verify embedder was called
    assert!(embedder.calls() > 0);
}

#[test]
fn test_harness_with_quality_embedder() {
    let harness = EvaluationHarness::new();
    let corpus = EvaluationCorpus::code_search_sample();
    let embedder = QualityFixtureEmbedder::new("quality-384", 384);

    let metadata = ModelMetadata {
        id: "quality-384".to_string(),
        name: "Quality Fixture".to_string(),
        source: "test".to_string(),
        release_date: "2025-12-01".to_string(),
        dimension: Some(384),
        size_bytes: Some(100_000_000),
        is_baseline: false,
    };

    let report = harness
        .evaluate(&embedder, &corpus, &metadata)
        .expect("evaluation should succeed");

    // Quality embedder should have reasonable NDCG
    assert!(
        report.ndcg_at_10 > 0.0,
        "Quality embedder should have non-zero NDCG"
    );
}

#[test]
fn test_harness_comparison() {
    let harness = EvaluationHarness::new();
    let corpus = EvaluationCorpus::code_search_sample();

    // Baseline
    let baseline_embedder = FixtureEmbedder::new("baseline-384", 384);
    let baseline_metadata = ModelMetadata {
        id: "baseline-384".to_string(),
        name: "Baseline".to_string(),
        source: "test".to_string(),
        release_date: "2022-01-01".to_string(),
        dimension: Some(384),
        size_bytes: Some(50_000_000),
        is_baseline: true,
    };

    // Candidate (using same type as baseline for type compatibility)
    let candidate_embedder = FixtureEmbedder::new("candidate-384", 384);
    let candidate_metadata = ModelMetadata {
        id: "candidate-384".to_string(),
        name: "Candidate".to_string(),
        source: "test".to_string(),
        release_date: "2025-12-01".to_string(),
        dimension: Some(384),
        size_bytes: Some(100_000_000),
        is_baseline: false,
    };

    let comparison = harness
        .run_comparison(
            (&baseline_embedder, &baseline_metadata),
            vec![(&candidate_embedder, &candidate_metadata)],
            &corpus,
        )
        .expect("comparison should succeed");

    // Verify comparison structure
    assert_eq!(comparison.baseline.model_id, "baseline-384");
    assert_eq!(comparison.candidates.len(), 1);
    assert!(!comparison.corpus_hash.is_empty());
    assert!(!comparison.recommendation_reason.is_empty());

    // Baseline should not be eligible (is_baseline = true)
    assert!(!comparison.baseline.eligible);
}

#[test]
fn test_format_comparison_table() {
    let harness = EvaluationHarness::new();
    let corpus = EvaluationCorpus::code_search_sample();

    let baseline_embedder = FixtureEmbedder::new("baseline", 384);
    let baseline_metadata = ModelMetadata {
        id: "baseline".to_string(),
        name: "Baseline".to_string(),
        source: "test".to_string(),
        release_date: "2022-01-01".to_string(),
        dimension: Some(384),
        size_bytes: Some(50_000_000),
        is_baseline: true,
    };

    let candidate_embedder = FixtureEmbedder::new("candidate", 384);
    let candidate_metadata = ModelMetadata {
        id: "candidate".to_string(),
        name: "Candidate".to_string(),
        source: "test".to_string(),
        release_date: "2025-12-01".to_string(),
        dimension: Some(384),
        size_bytes: Some(100_000_000),
        is_baseline: false,
    };

    let comparison = harness
        .run_comparison(
            (&baseline_embedder, &baseline_metadata),
            vec![(&candidate_embedder, &candidate_metadata)],
            &corpus,
        )
        .expect("comparison should succeed");

    let table = format_comparison_table(&comparison);

    // Verify table contains expected elements
    assert!(table.contains("Bake-off Results"));
    assert!(table.contains("NDCG@10"));
    assert!(table.contains("P50"));
    assert!(table.contains("P99"));
    assert!(table.contains("baseline"));
    assert!(table.contains("candidate"));
    assert!(table.contains("Recommendation"));
}

#[test]
fn test_custom_evaluation_config() {
    let config = EvaluationConfig {
        warmup_queries: 1,
        timing_iterations: 2,
        ndcg_k: 5,
    };
    let harness = EvaluationHarness::with_config(config);
    let corpus = EvaluationCorpus::code_search_sample();
    let embedder = FixtureEmbedder::new("test", 256);

    let metadata = ModelMetadata {
        id: "test".to_string(),
        name: "Test".to_string(),
        source: "test".to_string(),
        release_date: "2025-12-01".to_string(),
        dimension: Some(256),
        size_bytes: Some(10_000_000),
        is_baseline: false,
    };

    let report = harness
        .evaluate(&embedder, &corpus, &metadata)
        .expect("should succeed");
    assert!(!report.corpus_hash.is_empty());
}

#[test]
fn test_zero_ndcg_k_is_invalid_input_before_corpus_or_model_work() {
    let harness = EvaluationHarness::with_config(EvaluationConfig {
        warmup_queries: 1,
        timing_iterations: 1,
        ndcg_k: 0,
    });
    let corpus = EvaluationCorpus::new("empty");
    let embedder = FixtureEmbedder::new("test", 16);
    let metadata = ModelMetadata {
        id: "test".to_string(),
        name: "Test".to_string(),
        source: "test".to_string(),
        release_date: "2025-12-01".to_string(),
        dimension: Some(16),
        size_bytes: Some(0),
        is_baseline: false,
    };

    let error = harness
        .evaluate(&embedder, &corpus, &metadata)
        .expect_err("NDCG requires a positive ranking cutoff");
    assert_eq!(error, "invalid-input: ndcg-k-zero");
    assert_eq!(
        embedder.calls(),
        0,
        "config validation must run before corpus or model work"
    );
}

#[test]
fn test_corpus_hash_stability() {
    let corpus1 = EvaluationCorpus::code_search_sample();
    let corpus2 = EvaluationCorpus::code_search_sample();

    let hash1 = corpus1.compute_hash();
    let hash2 = corpus2.compute_hash();

    assert_eq!(hash1, hash2, "Same corpus should produce same hash");
}

#[test]
fn test_empty_corpus_error() {
    let harness = EvaluationHarness::new();
    let corpus = EvaluationCorpus::new("empty");
    let embedder = FixtureEmbedder::new("test", 256);

    let metadata = ModelMetadata {
        id: "test".to_string(),
        name: "Test".to_string(),
        source: "test".to_string(),
        release_date: "2025-12-01".to_string(),
        dimension: Some(256),
        size_bytes: Some(10_000_000),
        is_baseline: false,
    };

    let error = harness
        .evaluate(&embedder, &corpus, &metadata)
        .expect_err("empty corpus should return an explicit no-data error");
    assert_eq!(error, "no-data: empty-corpus");
    assert_eq!(
        embedder.calls(),
        0,
        "corpus validation must run before model work"
    );
}

#[test]
fn test_empty_judgments_are_no_data_before_model_work() {
    let harness = EvaluationHarness::new();
    let mut corpus = EvaluationCorpus::new("empty-judgments");
    corpus.add_document("d1", "document");
    corpus.add_query("query", vec![]);
    let embedder = FixtureEmbedder::new("test", 16);
    let metadata = ModelMetadata {
        id: "test".to_string(),
        name: "Test".to_string(),
        source: "test".to_string(),
        release_date: "2025-12-01".to_string(),
        dimension: Some(16),
        size_bytes: Some(0),
        is_baseline: false,
    };

    let error = harness
        .evaluate(&embedder, &corpus, &metadata)
        .expect_err("a query without judgments has no evaluation truth");
    assert_eq!(error, "no-data: empty-judgments:query-index=0");
    assert_eq!(embedder.calls(), 0);
}

#[test]
fn test_non_finite_relevance_is_invalid_input_before_model_work() {
    let harness = EvaluationHarness::new();
    let mut corpus = EvaluationCorpus::new("non-finite-relevance");
    corpus.add_document("d1", "document");
    corpus.add_query("query", vec![("d1", f64::NAN)]);
    let embedder = FixtureEmbedder::new("test", 16);
    let metadata = ModelMetadata {
        id: "test".to_string(),
        name: "Test".to_string(),
        source: "test".to_string(),
        release_date: "2025-12-01".to_string(),
        dimension: Some(16),
        size_bytes: Some(0),
        is_baseline: false,
    };

    let error = harness
        .evaluate(&embedder, &corpus, &metadata)
        .expect_err("non-finite relevance must not be normalized into zero quality");
    assert_eq!(
        error,
        "invalid-input: non-finite-relevance:query-index=0:judgment-index=0"
    );
    assert_eq!(embedder.calls(), 0);
}

#[test]
fn test_negative_relevance_is_invalid_input_before_model_work() {
    let harness = EvaluationHarness::new();
    let mut corpus = EvaluationCorpus::new("negative-relevance");
    corpus.add_document("d1", "document");
    corpus.add_query("query", vec![("d1", -1.0)]);
    let embedder = FixtureEmbedder::new("test", 16);
    let metadata = ModelMetadata {
        id: "test".to_string(),
        name: "Test".to_string(),
        source: "test".to_string(),
        release_date: "2025-12-01".to_string(),
        dimension: Some(16),
        size_bytes: Some(0),
        is_baseline: false,
    };

    let error = harness
        .evaluate(&embedder, &corpus, &metadata)
        .expect_err("negative relevance is outside the documented NDCG label domain");
    assert_eq!(
        error,
        "invalid-input: negative-relevance:query-index=0:judgment-index=0"
    );
    assert_eq!(embedder.calls(), 0);
}

#[test]
fn test_dangling_judgment_document_is_invalid_input_before_model_work() {
    let harness = EvaluationHarness::new();
    let mut corpus = EvaluationCorpus::new("dangling-judgment");
    corpus.add_document("d1", "document");
    corpus.add_query("query", vec![("missing", 3.0)]);
    let embedder = FixtureEmbedder::new("test", 16);
    let metadata = ModelMetadata {
        id: "test".to_string(),
        name: "Test".to_string(),
        source: "test".to_string(),
        release_date: "2025-12-01".to_string(),
        dimension: Some(16),
        size_bytes: Some(0),
        is_baseline: false,
    };

    let error = harness
        .evaluate(&embedder, &corpus, &metadata)
        .expect_err("judgments must refer to documents in the evaluated corpus");
    assert_eq!(
        error,
        "invalid-input: unknown-document-id:query-index=0:judgment-index=0"
    );
    assert_eq!(embedder.calls(), 0);
}

#[test]
fn test_all_zero_judgments_are_no_data_not_successful_zero_quality() {
    let harness = EvaluationHarness::new();
    let mut corpus = EvaluationCorpus::new("zero-ground-truth");
    corpus.add_document("d1", "document");
    corpus.add_query("query", vec![("d1", 0.0)]);
    let embedder = FixtureEmbedder::new("test", 16);
    let metadata = ModelMetadata {
        id: "test".to_string(),
        name: "Test".to_string(),
        source: "test".to_string(),
        release_date: "2025-12-01".to_string(),
        dimension: Some(16),
        size_bytes: Some(0),
        is_baseline: false,
    };

    let error = harness
        .evaluate(&embedder, &corpus, &metadata)
        .expect_err("all-zero ground truth has no NDCG denominator");
    assert_eq!(error, "no-data: no-positive-relevance:query-index=0");
    assert_eq!(embedder.calls(), 0);
}

#[test]
fn test_ineligible_by_date() {
    let harness = EvaluationHarness::new();
    let corpus = EvaluationCorpus::code_search_sample();
    let embedder = FixtureEmbedder::new("old", 384);

    let metadata = ModelMetadata {
        id: "old".to_string(),
        name: "Old Model".to_string(),
        source: "test".to_string(),
        release_date: "2025-06-01".to_string(), // Before cutoff
        dimension: Some(384),
        size_bytes: Some(50_000_000),
        is_baseline: false,
    };

    let report = harness
        .evaluate(&embedder, &corpus, &metadata)
        .expect("should succeed");
    assert!(
        !report.eligible,
        "Model before cutoff should not be eligible"
    );
}
