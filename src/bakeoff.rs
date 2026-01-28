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
}
