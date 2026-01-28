//! Integration tests for the bakeoff evaluation harness.
//!
//! These tests verify the harness works correctly with mock embedders.

use coding_agent_search::bakeoff::{
    EvaluationConfig, EvaluationCorpus, EvaluationHarness, ModelMetadata, format_comparison_table,
};
use coding_agent_search::search::embedder::{Embedder, EmbedderError, EmbedderResult};
use std::sync::atomic::{AtomicUsize, Ordering};

/// A mock embedder for testing the harness.
/// Returns deterministic embeddings based on text hash.
struct MockEmbedder {
    id: String,
    dimension: usize,
    call_count: AtomicUsize,
}

impl MockEmbedder {
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

impl Embedder for MockEmbedder {
    fn embed(&self, text: &str) -> EmbedderResult<Vec<f32>> {
        if text.is_empty() {
            return Err(EmbedderError::InvalidInput("empty text".to_string()));
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
}

/// A mock embedder that produces quality-aware embeddings.
/// Similar texts produce similar embeddings.
struct QualityMockEmbedder {
    id: String,
    dimension: usize,
}

impl QualityMockEmbedder {
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

impl Embedder for QualityMockEmbedder {
    fn embed(&self, text: &str) -> EmbedderResult<Vec<f32>> {
        if text.is_empty() {
            return Err(EmbedderError::InvalidInput("empty text".to_string()));
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
}

#[test]
fn test_harness_with_mock_embedder() {
    let harness = EvaluationHarness::new();
    let corpus = EvaluationCorpus::code_search_sample();
    let embedder = MockEmbedder::new("mock-384", 384);

    let metadata = ModelMetadata {
        id: "mock-384".to_string(),
        name: "Mock Embedder".to_string(),
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
    assert_eq!(report.model_id, "mock-384");
    assert!(!report.corpus_hash.is_empty());
    assert!(report.ndcg_at_10 >= 0.0 && report.ndcg_at_10 <= 1.0);
    // cold_start_ms can be 0 for very fast mock embedders (sub-millisecond)
    assert!(report.eligible); // Released 2025-12-01, after cutoff

    // Verify embedder was called
    assert!(embedder.calls() > 0);
}

#[test]
fn test_harness_with_quality_embedder() {
    let harness = EvaluationHarness::new();
    let corpus = EvaluationCorpus::code_search_sample();
    let embedder = QualityMockEmbedder::new("quality-384", 384);

    let metadata = ModelMetadata {
        id: "quality-384".to_string(),
        name: "Quality Mock".to_string(),
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
    let baseline_embedder = MockEmbedder::new("baseline-384", 384);
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
    let candidate_embedder = MockEmbedder::new("candidate-384", 384);
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

    let baseline_embedder = MockEmbedder::new("baseline", 384);
    let baseline_metadata = ModelMetadata {
        id: "baseline".to_string(),
        name: "Baseline".to_string(),
        source: "test".to_string(),
        release_date: "2022-01-01".to_string(),
        dimension: Some(384),
        size_bytes: Some(50_000_000),
        is_baseline: true,
    };

    let candidate_embedder = MockEmbedder::new("candidate", 384);
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
    let embedder = MockEmbedder::new("test", 256);

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
    let embedder = MockEmbedder::new("test", 256);

    let metadata = ModelMetadata {
        id: "test".to_string(),
        name: "Test".to_string(),
        source: "test".to_string(),
        release_date: "2025-12-01".to_string(),
        dimension: Some(256),
        size_bytes: Some(10_000_000),
        is_baseline: false,
    };

    let result = harness.evaluate(&embedder, &corpus, &metadata);
    assert!(result.is_err(), "Empty corpus should return error");
    assert!(result.unwrap_err().contains("Empty corpus"));
}

#[test]
fn test_ineligible_by_date() {
    let harness = EvaluationHarness::new();
    let corpus = EvaluationCorpus::code_search_sample();
    let embedder = MockEmbedder::new("old", 384);

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
