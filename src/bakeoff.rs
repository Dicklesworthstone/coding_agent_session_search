use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::time::{Duration, Instant};

/// Hard eligibility cutoff: models must be released on/after this date.
/// Format: YYYY-MM-DD
pub const ELIGIBILITY_CUTOFF: &str = "2025-11-01";

/// Success criteria from the epic.
pub mod criteria {
    /// Cold start must be under 2 seconds.
    pub const COLD_START_MAX_MS: u64 = 2000;
    /// Warm p99 latency must be under 250ms.
    pub const WARM_P99_MAX_MS: u64 = 250;
    /// Memory usage must be under 300MB per model.
    pub const MEMORY_MAX_MB: u64 = 300;
    /// Quality must be at least 80% of baseline (MiniLM).
    pub const QUALITY_MIN_RATIO: f64 = 0.80;
}

/// Model metadata for eligibility checking.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelMetadata {
    /// Model identifier (e.g., "bge-small-en-v1.5").
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// HuggingFace model ID or source.
    pub source: String,
    /// Release/update date (YYYY-MM-DD format).
    pub release_date: String,
    /// Embedding dimension (for embedders).
    pub dimension: Option<usize>,
    /// Model size in bytes.
    pub size_bytes: Option<u64>,
    /// Whether this is a baseline model (not eligible to win, but used for comparison).
    pub is_baseline: bool,
}

impl ModelMetadata {
    /// Check if the model is eligible based on release date.
    pub fn is_eligible(&self) -> bool {
        if self.is_baseline {
            return false;
        }
        self.release_date.as_str() >= ELIGIBILITY_CUTOFF
    }
}

/// Minimal validation report for bake-off runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValidationReport {
    pub model_id: String,
    pub corpus_hash: String,
    pub ndcg_at_10: f64,
    pub latency_ms_p50: u64,
    pub latency_ms_p95: u64,
    pub latency_ms_p99: u64,
    pub cold_start_ms: u64,
    pub memory_mb: u64,
    pub eligible: bool,
    pub meets_criteria: bool,
    pub warnings: Vec<String>,
}

impl ValidationReport {
    /// Check if this report meets all success criteria.
    pub fn check_criteria(&self) -> bool {
        self.cold_start_ms <= criteria::COLD_START_MAX_MS
            && self.latency_ms_p99 <= criteria::WARM_P99_MAX_MS
            && self.memory_mb <= criteria::MEMORY_MAX_MB
    }

    /// Check quality against a baseline report.
    pub fn meets_quality_threshold(&self, baseline: &ValidationReport) -> bool {
        if baseline.ndcg_at_10 == 0.0 {
            return true;
        }
        self.ndcg_at_10 / baseline.ndcg_at_10 >= criteria::QUALITY_MIN_RATIO
    }
}

/// Latency statistics from a benchmark run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LatencyStats {
    pub samples: usize,
    pub min_ms: u64,
    pub max_ms: u64,
    pub mean_ms: f64,
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub p99_ms: u64,
}

impl LatencyStats {
    /// Compute latency statistics from a list of durations.
    pub fn from_durations(durations: &[Duration]) -> Self {
        if durations.is_empty() {
            return Self {
                samples: 0,
                min_ms: 0,
                max_ms: 0,
                mean_ms: 0.0,
                p50_ms: 0,
                p95_ms: 0,
                p99_ms: 0,
            };
        }

        let mut millis: Vec<u64> = durations.iter().map(|d| d.as_millis() as u64).collect();
        millis.sort_unstable();

        let n = millis.len();
        let sum: u64 = millis.iter().sum();

        Self {
            samples: n,
            min_ms: millis[0],
            max_ms: millis[n - 1],
            mean_ms: sum as f64 / n as f64,
            p50_ms: percentile(&millis, 50),
            p95_ms: percentile(&millis, 95),
            p99_ms: percentile(&millis, 99),
        }
    }
}

/// Compute percentile from sorted values.
fn percentile(sorted: &[u64], p: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = (p * sorted.len() / 100).min(sorted.len() - 1);
    sorted[idx]
}

/// Timer for measuring operation latency.
pub struct LatencyTimer {
    samples: Vec<Duration>,
}

impl LatencyTimer {
    pub fn new() -> Self {
        Self {
            samples: Vec::new(),
        }
    }

    /// Time a single operation and record the duration.
    pub fn time<F, T>(&mut self, f: F) -> T
    where
        F: FnOnce() -> T,
    {
        let start = Instant::now();
        let result = f();
        self.samples.push(start.elapsed());
        result
    }

    /// Get statistics from recorded samples.
    pub fn stats(&self) -> LatencyStats {
        LatencyStats::from_durations(&self.samples)
    }

    /// Clear recorded samples.
    pub fn clear(&mut self) {
        self.samples.clear();
    }
}

impl Default for LatencyTimer {
    fn default() -> Self {
        Self::new()
    }
}

/// Bake-off comparison result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BakeoffComparison {
    /// Corpus hash for reproducibility.
    pub corpus_hash: String,
    /// Baseline model report.
    pub baseline: ValidationReport,
    /// All candidate reports.
    pub candidates: Vec<ValidationReport>,
    /// Recommended model ID (best eligible candidate meeting criteria).
    pub recommendation: Option<String>,
    /// Reason for recommendation.
    pub recommendation_reason: String,
}

impl BakeoffComparison {
    /// Find the best eligible candidate that meets all criteria.
    pub fn find_winner(&self) -> Option<&ValidationReport> {
        self.candidates
            .iter()
            .filter(|r| r.eligible && r.meets_criteria && r.meets_quality_threshold(&self.baseline))
            .max_by(|a, b| {
                // Prefer higher quality, then lower latency
                a.ndcg_at_10
                    .partial_cmp(&b.ndcg_at_10)
                    .unwrap_or(Ordering::Equal)
                    .then_with(|| b.latency_ms_p99.cmp(&a.latency_ms_p99))
            })
    }
}

/// Compute NDCG@k for a list of relevances in rank order.
/// Non-finite or <= 0 relevances are treated as non-relevant.
pub fn ndcg_at_k(relevances: &[f64], k: usize) -> f64 {
    if k == 0 || relevances.is_empty() {
        return 0.0;
    }
    let dcg = dcg_at_k(relevances, k);
    if dcg == 0.0 {
        return 0.0;
    }
    let mut ideal: Vec<f64> = relevances
        .iter()
        .map(|rel| if rel.is_finite() { rel.max(0.0) } else { 0.0 })
        .collect();
    ideal.sort_by(|a, b| b.partial_cmp(a).unwrap_or(Ordering::Equal));
    let idcg = dcg_at_k(&ideal, k);
    if idcg == 0.0 { 0.0 } else { dcg / idcg }
}

fn dcg_at_k(relevances: &[f64], k: usize) -> f64 {
    relevances
        .iter()
        .take(k)
        .enumerate()
        .map(|(idx, rel)| {
            let rel = if rel.is_finite() { *rel } else { 0.0 };
            let rel = rel.max(0.0);
            let denom = (idx as f64 + 2.0).log2();
            (2.0_f64.powf(rel) - 1.0) / denom
        })
        .sum()
}

// ==================== Evaluation Harness ====================

/// A document in the evaluation corpus.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Document {
    /// Unique document identifier.
    pub id: String,
    /// Document content (text to embed).
    pub content: String,
}

/// Ground truth relevance judgment for a query-document pair.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelevanceJudgment {
    /// Document ID.
    pub doc_id: String,
    /// Relevance score (0=not relevant, 1=somewhat, 2=highly, 3=perfect).
    pub relevance: f64,
}

/// A query with ground truth relevance judgments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryWithJudgments {
    /// Query text.
    pub query: String,
    /// Ground truth relevance judgments for this query.
    pub judgments: Vec<RelevanceJudgment>,
}

/// Evaluation corpus containing documents and queries with ground truth.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvaluationCorpus {
    /// Corpus name/identifier.
    pub name: String,
    /// Documents in the corpus.
    pub documents: Vec<Document>,
    /// Queries with ground truth judgments.
    pub queries: Vec<QueryWithJudgments>,
}

impl EvaluationCorpus {
    /// Create a new empty corpus.
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            documents: Vec::new(),
            queries: Vec::new(),
        }
    }

    /// Add a document to the corpus.
    pub fn add_document(&mut self, id: &str, content: &str) {
        self.documents.push(Document {
            id: id.to_string(),
            content: content.to_string(),
        });
    }

    /// Add a query with judgments.
    pub fn add_query(&mut self, query: &str, judgments: Vec<(&str, f64)>) {
        self.queries.push(QueryWithJudgments {
            query: query.to_string(),
            judgments: judgments
                .into_iter()
                .map(|(doc_id, relevance)| RelevanceJudgment {
                    doc_id: doc_id.to_string(),
                    relevance,
                })
                .collect(),
        });
    }

    /// Compute a hash of the corpus for reproducibility.
    pub fn compute_hash(&self) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        self.name.hash(&mut hasher);
        for doc in &self.documents {
            doc.id.hash(&mut hasher);
            doc.content.hash(&mut hasher);
        }
        for query in &self.queries {
            query.query.hash(&mut hasher);
            for j in &query.judgments {
                j.doc_id.hash(&mut hasher);
                // Hash relevance as bits to avoid float issues
                j.relevance.to_bits().hash(&mut hasher);
            }
        }
        format!("{:016x}", hasher.finish())
    }

    /// Create a sample corpus for testing embedders on code search scenarios.
    pub fn code_search_sample() -> Self {
        let mut corpus = Self::new("code-search-sample");

        // Add sample documents representing code snippets and discussions
        corpus.add_document("d1", "implementing authentication with jwt tokens in rust using jsonwebtoken crate for secure api access");
        corpus.add_document("d2", "database connection pool configuration using sqlx with postgres for high performance queries");
        corpus.add_document(
            "d3",
            "error handling patterns in rust using thiserror and anyhow for better error messages",
        );
        corpus.add_document(
            "d4",
            "async runtime setup with tokio for concurrent task processing and io operations",
        );
        corpus.add_document(
            "d5",
            "parsing json data with serde for serialization and deserialization of structs",
        );
        corpus.add_document(
            "d6",
            "logging configuration using tracing crate for structured observability and debugging",
        );
        corpus.add_document(
            "d7",
            "cli argument parsing with clap for building command line applications",
        );
        corpus.add_document(
            "d8",
            "http client requests using reqwest for making api calls to external services",
        );
        corpus.add_document(
            "d9",
            "unit testing patterns with cargo test and mock objects for reliable tests",
        );
        corpus.add_document(
            "d10",
            "file system operations reading and writing files with std fs module",
        );

        // Add queries with ground truth relevance judgments
        // Relevance: 0=not relevant, 1=somewhat, 2=highly, 3=perfect match
        corpus.add_query(
            "how to authenticate users with jwt",
            vec![
                ("d1", 3.0), // Perfect match
                ("d2", 0.0), // Not relevant
                ("d8", 1.0), // Somewhat (might involve API auth)
            ],
        );

        corpus.add_query(
            "database connection setup",
            vec![
                ("d2", 3.0),  // Perfect match
                ("d4", 1.0),  // Async might be related
                ("d10", 0.0), // Not relevant
            ],
        );

        corpus.add_query(
            "error handling best practices",
            vec![
                ("d3", 3.0), // Perfect match
                ("d6", 1.0), // Logging errors
                ("d9", 1.0), // Testing error cases
            ],
        );

        corpus.add_query(
            "async programming tokio",
            vec![
                ("d4", 3.0), // Perfect match
                ("d2", 1.0), // Async DB queries
                ("d8", 2.0), // Async HTTP
            ],
        );

        corpus.add_query(
            "json serialization",
            vec![
                ("d5", 3.0), // Perfect match
                ("d8", 1.0), // API often uses JSON
                ("d1", 1.0), // JWT is JSON-based
            ],
        );

        corpus
    }
}

/// Result of evaluating a single query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryEvalResult {
    /// The query text.
    pub query: String,
    /// NDCG@10 for this query.
    pub ndcg_at_10: f64,
    /// Ranked document IDs returned by the model.
    pub ranked_docs: Vec<String>,
    /// Latency for this query in milliseconds.
    pub latency_ms: u64,
}

/// Configuration for the evaluation harness.
#[derive(Debug, Clone)]
pub struct EvaluationConfig {
    /// Number of warmup queries before timing.
    pub warmup_queries: usize,
    /// Number of timing iterations per query.
    pub timing_iterations: usize,
    /// Top-k for NDCG calculation.
    pub ndcg_k: usize,
}

impl Default for EvaluationConfig {
    fn default() -> Self {
        Self {
            warmup_queries: 3,
            timing_iterations: 5,
            ndcg_k: 10,
        }
    }
}

/// Compute cosine similarity between two vectors.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

/// Evaluation harness for running bake-off evaluations.
pub struct EvaluationHarness {
    config: EvaluationConfig,
}

impl EvaluationHarness {
    /// Create a new evaluation harness with default config.
    pub fn new() -> Self {
        Self {
            config: EvaluationConfig::default(),
        }
    }

    /// Create with custom config.
    pub fn with_config(config: EvaluationConfig) -> Self {
        Self { config }
    }

    /// Evaluate an embedder against a corpus.
    ///
    /// Returns a ValidationReport with NDCG, latency, and memory metrics.
    pub fn evaluate<E: crate::search::embedder::Embedder>(
        &self,
        embedder: &E,
        corpus: &EvaluationCorpus,
        metadata: &ModelMetadata,
    ) -> Result<ValidationReport, String> {
        let corpus_hash = corpus.compute_hash();

        // Measure cold start (first embedding)
        let cold_start = Instant::now();
        let first_doc = corpus.documents.first().ok_or("Empty corpus")?;
        embedder
            .embed(&first_doc.content)
            .map_err(|e| e.to_string())?;
        let cold_start_ms = cold_start.elapsed().as_millis() as u64;

        // Embed all documents
        let doc_embeddings: Vec<Vec<f32>> = corpus
            .documents
            .iter()
            .map(|d| embedder.embed(&d.content))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;

        // Warmup queries
        for i in 0..self.config.warmup_queries.min(corpus.queries.len()) {
            let _ = embedder.embed(&corpus.queries[i].query);
        }

        // Evaluate each query
        let mut query_results = Vec::new();
        let mut latencies = Vec::new();

        for query_with_judgments in &corpus.queries {
            // Build relevance map
            let relevance_map: std::collections::HashMap<&str, f64> = query_with_judgments
                .judgments
                .iter()
                .map(|j| (j.doc_id.as_str(), j.relevance))
                .collect();

            // Time the query embedding (average over iterations, minimum 1)
            let iterations = self.config.timing_iterations.max(1);
            let mut query_latencies = Vec::with_capacity(iterations);
            let mut query_embedding = Vec::new();
            for _ in 0..iterations {
                let start = Instant::now();
                query_embedding = embedder
                    .embed(&query_with_judgments.query)
                    .map_err(|e| e.to_string())?;
                query_latencies.push(start.elapsed());
            }
            let avg_latency = query_latencies
                .iter()
                .map(|d| d.as_millis() as u64)
                .sum::<u64>()
                / query_latencies.len() as u64;
            latencies.push(Duration::from_millis(avg_latency));

            // Rank documents by similarity
            let mut scored_docs: Vec<(usize, f32)> = doc_embeddings
                .iter()
                .enumerate()
                .map(|(idx, emb)| (idx, cosine_similarity(&query_embedding, emb)))
                .collect();
            scored_docs.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

            // Get ranked doc IDs
            let ranked_docs: Vec<String> = scored_docs
                .iter()
                .take(self.config.ndcg_k)
                .map(|(idx, _)| corpus.documents[*idx].id.clone())
                .collect();

            // Compute relevances in ranked order
            let relevances: Vec<f64> = ranked_docs
                .iter()
                .map(|id| *relevance_map.get(id.as_str()).unwrap_or(&0.0))
                .collect();

            let ndcg = ndcg_at_k(&relevances, self.config.ndcg_k);

            query_results.push(QueryEvalResult {
                query: query_with_judgments.query.clone(),
                ndcg_at_10: ndcg,
                ranked_docs,
                latency_ms: avg_latency,
            });
        }

        // Compute aggregate metrics
        let avg_ndcg = if query_results.is_empty() {
            0.0
        } else {
            query_results.iter().map(|r| r.ndcg_at_10).sum::<f64>() / query_results.len() as f64
        };

        let latency_stats = LatencyStats::from_durations(&latencies);

        // Estimate memory (model size as proxy - real measurement would need system APIs)
        let memory_mb = metadata.size_bytes.unwrap_or(0) / (1024 * 1024);

        let eligible = metadata.is_eligible();
        let mut report = ValidationReport {
            model_id: metadata.id.clone(),
            corpus_hash,
            ndcg_at_10: avg_ndcg,
            latency_ms_p50: latency_stats.p50_ms,
            latency_ms_p95: latency_stats.p95_ms,
            latency_ms_p99: latency_stats.p99_ms,
            cold_start_ms,
            memory_mb,
            eligible,
            meets_criteria: false,
            warnings: Vec::new(),
        };

        report.meets_criteria = report.check_criteria();

        // Add warnings
        if cold_start_ms > criteria::COLD_START_MAX_MS {
            report.warnings.push(format!(
                "Cold start {}ms exceeds {}ms limit",
                cold_start_ms,
                criteria::COLD_START_MAX_MS
            ));
        }
        if latency_stats.p99_ms > criteria::WARM_P99_MAX_MS {
            report.warnings.push(format!(
                "P99 latency {}ms exceeds {}ms limit",
                latency_stats.p99_ms,
                criteria::WARM_P99_MAX_MS
            ));
        }
        if memory_mb > criteria::MEMORY_MAX_MB {
            report.warnings.push(format!(
                "Memory {}MB exceeds {}MB limit",
                memory_mb,
                criteria::MEMORY_MAX_MB
            ));
        }

        Ok(report)
    }

    /// Run a full bake-off comparison with baseline and candidates.
    pub fn run_comparison<E: crate::search::embedder::Embedder>(
        &self,
        baseline: (&E, &ModelMetadata),
        candidates: Vec<(&E, &ModelMetadata)>,
        corpus: &EvaluationCorpus,
    ) -> Result<BakeoffComparison, String> {
        let corpus_hash = corpus.compute_hash();

        // Evaluate baseline
        let baseline_report = self.evaluate(baseline.0, corpus, baseline.1)?;

        // Evaluate all candidates
        let mut candidate_reports = Vec::new();
        for (embedder, metadata) in candidates {
            let report = self.evaluate(embedder, corpus, metadata)?;
            candidate_reports.push(report);
        }

        // Build initial comparison
        let mut comparison = BakeoffComparison {
            corpus_hash,
            baseline: baseline_report.clone(),
            candidates: candidate_reports,
            recommendation: None,
            recommendation_reason: String::new(),
        };

        // Find the winner and extract data before mutating
        let winner_data = comparison.find_winner().map(|w| {
            (
                w.model_id.clone(),
                w.ndcg_at_10,
                w.latency_ms_p99,
                w.memory_mb,
            )
        });

        if let Some((model_id, ndcg, p99, memory)) = winner_data {
            comparison.recommendation = Some(model_id.clone());
            comparison.recommendation_reason = format!(
                "Best eligible candidate with NDCG@10={:.3} ({}% of baseline), p99={}ms, memory={}MB",
                ndcg,
                (ndcg / baseline_report.ndcg_at_10 * 100.0) as u32,
                p99,
                memory
            );
        } else {
            comparison.recommendation_reason =
                "No eligible candidate meets all criteria".to_string();
        }

        Ok(comparison)
    }
}

impl Default for EvaluationHarness {
    fn default() -> Self {
        Self::new()
    }
}

/// Format a comparison as a markdown table for reporting.
pub fn format_comparison_table(comparison: &BakeoffComparison) -> String {
    let mut output = String::new();

    output.push_str("# Bake-off Results\n\n");
    output.push_str(&format!("Corpus hash: `{}`\n\n", comparison.corpus_hash));

    output.push_str("| Model | NDCG@10 | P50 (ms) | P95 (ms) | P99 (ms) | Cold (ms) | Memory (MB) | Eligible | Meets Criteria |\n");
    output.push_str("|-------|---------|----------|----------|----------|-----------|-------------|----------|----------------|\n");

    // Baseline first
    let b = &comparison.baseline;
    output.push_str(&format!(
        "| {} (baseline) | {:.3} | {} | {} | {} | {} | {} | {} | {} |\n",
        b.model_id,
        b.ndcg_at_10,
        b.latency_ms_p50,
        b.latency_ms_p95,
        b.latency_ms_p99,
        b.cold_start_ms,
        b.memory_mb,
        if b.eligible { "✓" } else { "✗" },
        if b.meets_criteria { "✓" } else { "✗" }
    ));

    // Candidates
    for c in &comparison.candidates {
        let marker = if Some(&c.model_id) == comparison.recommendation.as_ref() {
            " ⭐"
        } else {
            ""
        };
        output.push_str(&format!(
            "| {}{} | {:.3} | {} | {} | {} | {} | {} | {} | {} |\n",
            c.model_id,
            marker,
            c.ndcg_at_10,
            c.latency_ms_p50,
            c.latency_ms_p95,
            c.latency_ms_p99,
            c.cold_start_ms,
            c.memory_mb,
            if c.eligible { "✓" } else { "✗" },
            if c.meets_criteria { "✓" } else { "✗" }
        ));
    }

    output.push_str("\n## Recommendation\n\n");
    if let Some(ref winner) = comparison.recommendation {
        output.push_str(&format!("**Winner:** {}\n\n", winner));
    }
    output.push_str(&format!("{}\n", comparison.recommendation_reason));

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ndcg_perfect_is_one() {
        let relevances = vec![3.0, 2.0, 1.0];
        let ndcg = ndcg_at_k(&relevances, 3);
        assert!((ndcg - 1.0).abs() < 1e-9);
    }

    #[test]
    fn ndcg_zero_when_no_relevance() {
        let relevances = vec![0.0, 0.0, 0.0];
        let ndcg = ndcg_at_k(&relevances, 3);
        assert_eq!(ndcg, 0.0);
    }

    #[test]
    fn ndcg_handles_partial_relevance() {
        let relevances = vec![1.0, 0.0, 2.0];
        let ndcg = ndcg_at_k(&relevances, 3);
        assert!(ndcg > 0.0 && ndcg < 1.0);
    }

    #[test]
    fn report_roundtrip() {
        let report = ValidationReport {
            model_id: "hash".to_string(),
            corpus_hash: "deadbeef".to_string(),
            ndcg_at_10: 0.42,
            latency_ms_p50: 12,
            latency_ms_p95: 30,
            latency_ms_p99: 45,
            cold_start_ms: 500,
            memory_mb: 150,
            eligible: true,
            meets_criteria: true,
            warnings: vec!["example warning".to_string()],
        };
        let encoded = serde_json::to_string(&report).expect("serialize");
        let decoded: ValidationReport = serde_json::from_str(&encoded).expect("deserialize");
        assert_eq!(report, decoded);
    }

    #[test]
    fn model_eligibility_by_date() {
        let eligible_model = ModelMetadata {
            id: "new-model".to_string(),
            name: "New Model".to_string(),
            source: "huggingface".to_string(),
            release_date: "2025-12-01".to_string(),
            dimension: Some(384),
            size_bytes: Some(100_000_000),
            is_baseline: false,
        };
        assert!(eligible_model.is_eligible());

        let old_model = ModelMetadata {
            id: "old-model".to_string(),
            name: "Old Model".to_string(),
            source: "huggingface".to_string(),
            release_date: "2025-06-01".to_string(),
            dimension: Some(384),
            size_bytes: Some(100_000_000),
            is_baseline: false,
        };
        assert!(!old_model.is_eligible());

        let baseline_model = ModelMetadata {
            id: "baseline".to_string(),
            name: "Baseline".to_string(),
            source: "huggingface".to_string(),
            release_date: "2025-12-01".to_string(),
            dimension: Some(384),
            size_bytes: Some(100_000_000),
            is_baseline: true,
        };
        assert!(!baseline_model.is_eligible());
    }

    #[test]
    fn latency_stats_from_durations() {
        let durations = vec![
            Duration::from_millis(10),
            Duration::from_millis(20),
            Duration::from_millis(30),
            Duration::from_millis(40),
            Duration::from_millis(100),
        ];
        let stats = LatencyStats::from_durations(&durations);

        assert_eq!(stats.samples, 5);
        assert_eq!(stats.min_ms, 10);
        assert_eq!(stats.max_ms, 100);
        assert!((stats.mean_ms - 40.0).abs() < 0.1);
        assert_eq!(stats.p50_ms, 30);
    }

    #[test]
    fn latency_stats_empty() {
        let stats = LatencyStats::from_durations(&[]);
        assert_eq!(stats.samples, 0);
        assert_eq!(stats.p50_ms, 0);
    }

    #[test]
    fn latency_timer_records_samples() {
        let mut timer = LatencyTimer::new();

        // Time a simple operation
        let result = timer.time(|| 42);
        assert_eq!(result, 42);

        let stats = timer.stats();
        assert_eq!(stats.samples, 1);
    }

    #[test]
    fn report_meets_criteria() {
        let good_report = ValidationReport {
            model_id: "good".to_string(),
            corpus_hash: "test".to_string(),
            ndcg_at_10: 0.85,
            latency_ms_p50: 50,
            latency_ms_p95: 100,
            latency_ms_p99: 200, // Under 250ms
            cold_start_ms: 1500, // Under 2s
            memory_mb: 200,      // Under 300MB
            eligible: true,
            meets_criteria: true,
            warnings: vec![],
        };
        assert!(good_report.check_criteria());

        let bad_latency = ValidationReport {
            latency_ms_p99: 300, // Over 250ms
            ..good_report.clone()
        };
        assert!(!bad_latency.check_criteria());

        let bad_cold_start = ValidationReport {
            cold_start_ms: 3000, // Over 2s
            ..good_report.clone()
        };
        assert!(!bad_cold_start.check_criteria());

        let bad_memory = ValidationReport {
            memory_mb: 400, // Over 300MB
            ..good_report
        };
        assert!(!bad_memory.check_criteria());
    }

    #[test]
    fn report_quality_threshold() {
        let baseline = ValidationReport {
            model_id: "baseline".to_string(),
            corpus_hash: "test".to_string(),
            ndcg_at_10: 0.80,
            latency_ms_p50: 50,
            latency_ms_p95: 100,
            latency_ms_p99: 150,
            cold_start_ms: 1000,
            memory_mb: 200,
            eligible: false,
            meets_criteria: true,
            warnings: vec![],
        };

        let good_candidate = ValidationReport {
            model_id: "good".to_string(),
            ndcg_at_10: 0.70, // 87.5% of baseline, above 80%
            ..baseline.clone()
        };
        assert!(good_candidate.meets_quality_threshold(&baseline));

        let bad_candidate = ValidationReport {
            model_id: "bad".to_string(),
            ndcg_at_10: 0.60, // 75% of baseline, below 80%
            ..baseline.clone()
        };
        assert!(!bad_candidate.meets_quality_threshold(&baseline));
    }

    #[test]
    fn bakeoff_comparison_finds_winner() {
        let baseline = ValidationReport {
            model_id: "baseline".to_string(),
            corpus_hash: "test".to_string(),
            ndcg_at_10: 0.80,
            latency_ms_p50: 50,
            latency_ms_p95: 100,
            latency_ms_p99: 150,
            cold_start_ms: 1000,
            memory_mb: 200,
            eligible: false,
            meets_criteria: true,
            warnings: vec![],
        };

        let candidate1 = ValidationReport {
            model_id: "candidate1".to_string(),
            ndcg_at_10: 0.75, // Good quality
            eligible: true,
            meets_criteria: true,
            ..baseline.clone()
        };

        let candidate2 = ValidationReport {
            model_id: "candidate2".to_string(),
            ndcg_at_10: 0.85, // Better quality
            eligible: true,
            meets_criteria: true,
            ..baseline.clone()
        };

        let ineligible = ValidationReport {
            model_id: "ineligible".to_string(),
            ndcg_at_10: 0.90, // Best quality but not eligible
            eligible: false,
            meets_criteria: true,
            ..baseline.clone()
        };

        let comparison = BakeoffComparison {
            corpus_hash: "test".to_string(),
            baseline: baseline.clone(),
            candidates: vec![candidate1, candidate2.clone(), ineligible],
            recommendation: None,
            recommendation_reason: String::new(),
        };

        let winner = comparison.find_winner();
        assert!(winner.is_some());
        assert_eq!(winner.unwrap().model_id, "candidate2");
    }

    // ==================== Harness Tests ====================

    #[test]
    fn corpus_creation_and_hash() {
        let mut corpus = EvaluationCorpus::new("test-corpus");
        corpus.add_document("d1", "hello world");
        corpus.add_document("d2", "goodbye world");
        corpus.add_query("hello", vec![("d1", 3.0), ("d2", 0.0)]);

        assert_eq!(corpus.name, "test-corpus");
        assert_eq!(corpus.documents.len(), 2);
        assert_eq!(corpus.queries.len(), 1);

        let hash1 = corpus.compute_hash();
        assert_eq!(hash1.len(), 16); // 16 hex chars

        // Same corpus should produce same hash
        let hash2 = corpus.compute_hash();
        assert_eq!(hash1, hash2);

        // Different corpus should produce different hash
        corpus.add_document("d3", "new document");
        let hash3 = corpus.compute_hash();
        assert_ne!(hash1, hash3);
    }

    #[test]
    fn sample_corpus_is_valid() {
        let corpus = EvaluationCorpus::code_search_sample();
        assert!(!corpus.documents.is_empty());
        assert!(!corpus.queries.is_empty());

        // Each query should have at least one judgment
        for query in &corpus.queries {
            assert!(!query.judgments.is_empty());
        }

        // Hash should be stable
        let hash = corpus.compute_hash();
        assert!(!hash.is_empty());
    }

    #[test]
    fn cosine_similarity_identical_vectors() {
        let v = vec![1.0, 2.0, 3.0];
        let sim = cosine_similarity(&v, &v);
        assert!((sim - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_orthogonal_vectors() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert!(sim.abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_opposite_vectors() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![-1.0, -2.0, -3.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim + 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_different_lengths() {
        let a = vec![1.0, 2.0];
        let b = vec![1.0, 2.0, 3.0];
        let sim = cosine_similarity(&a, &b);
        assert_eq!(sim, 0.0);
    }

    #[test]
    fn evaluation_config_defaults() {
        let config = EvaluationConfig::default();
        assert_eq!(config.warmup_queries, 3);
        assert_eq!(config.timing_iterations, 5);
        assert_eq!(config.ndcg_k, 10);
    }

    #[test]
    fn harness_creation() {
        let harness = EvaluationHarness::new();
        assert_eq!(harness.config.ndcg_k, 10);

        let custom_config = EvaluationConfig {
            warmup_queries: 5,
            timing_iterations: 10,
            ndcg_k: 5,
        };
        let harness = EvaluationHarness::with_config(custom_config);
        assert_eq!(harness.config.ndcg_k, 5);
    }

    #[test]
    fn corpus_roundtrip() {
        let corpus = EvaluationCorpus::code_search_sample();
        let json = serde_json::to_string(&corpus).expect("serialize");
        let decoded: EvaluationCorpus = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(corpus, decoded);
    }

    #[test]
    fn query_eval_result_roundtrip() {
        let result = QueryEvalResult {
            query: "test query".to_string(),
            ndcg_at_10: 0.85,
            ranked_docs: vec!["d1".to_string(), "d2".to_string()],
            latency_ms: 15,
        };
        let json = serde_json::to_string(&result).expect("serialize");
        let decoded: QueryEvalResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(result.query, decoded.query);
        assert_eq!(result.ndcg_at_10, decoded.ndcg_at_10);
    }

    #[test]
    fn format_comparison_table_output() {
        let baseline = ValidationReport {
            model_id: "baseline".to_string(),
            corpus_hash: "test123".to_string(),
            ndcg_at_10: 0.80,
            latency_ms_p50: 50,
            latency_ms_p95: 100,
            latency_ms_p99: 150,
            cold_start_ms: 1000,
            memory_mb: 200,
            eligible: false,
            meets_criteria: true,
            warnings: vec![],
        };

        let candidate = ValidationReport {
            model_id: "winner".to_string(),
            ndcg_at_10: 0.85,
            eligible: true,
            meets_criteria: true,
            ..baseline.clone()
        };

        let comparison = BakeoffComparison {
            corpus_hash: "test123".to_string(),
            baseline,
            candidates: vec![candidate],
            recommendation: Some("winner".to_string()),
            recommendation_reason: "Best candidate".to_string(),
        };

        let table = format_comparison_table(&comparison);
        assert!(table.contains("Bake-off Results"));
        assert!(table.contains("baseline"));
        assert!(table.contains("winner"));
        assert!(table.contains("⭐")); // Winner marker
        assert!(table.contains("Recommendation"));
    }
}
