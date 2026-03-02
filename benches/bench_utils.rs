#![allow(dead_code, unused_imports)]
//! Shared benchmark utilities for criterion benchmarks.
//!
//! This module provides common utilities for configuring criterion benchmarks
//! and running scaling tests across the codebase.

use criterion::{BenchmarkId, Criterion, PlotConfiguration, SamplingMode, Throughput};
use std::time::Duration;

/// Configure criterion with optimized settings for the CASS benchmarks.
///
/// Returns a `Criterion` instance with settings tuned for reliable measurement
/// of search and indexing operations.
///
/// # Settings
/// - Sample size: 50 (balanced accuracy vs speed)
/// - Measurement time: 3 seconds per benchmark
/// - Warm-up time: 1 second
/// - Noise threshold: 3% (ignore small variations)
/// - Confidence level: 95%
pub fn configure_criterion() -> Criterion {
    Criterion::default()
        .sample_size(50)
        .measurement_time(Duration::from_secs(3))
        .warm_up_time(Duration::from_secs(1))
        .noise_threshold(0.03)
        .confidence_level(0.95)
        .without_plots() // Disable plots for CI environments
}

/// Configure criterion for quick benchmarks (development iteration).
///
/// Uses fewer samples and shorter measurement time for faster feedback.
pub fn configure_criterion_quick() -> Criterion {
    Criterion::default()
        .sample_size(20)
        .measurement_time(Duration::from_secs(1))
        .warm_up_time(Duration::from_millis(500))
        .noise_threshold(0.05)
        .without_plots()
}

/// Configure criterion for thorough benchmarks (CI/release validation).
///
/// Uses more samples and longer measurement time for higher accuracy.
pub fn configure_criterion_thorough() -> Criterion {
    Criterion::default()
        .sample_size(100)
        .measurement_time(Duration::from_secs(5))
        .warm_up_time(Duration::from_secs(2))
        .noise_threshold(0.02)
        .confidence_level(0.99)
        .with_plots()
}

/// Standard corpus sizes for scaling benchmarks.
pub const SCALING_SIZES: &[usize] = &[1_000, 5_000, 10_000, 25_000, 50_000];

/// Small corpus sizes for quick scaling tests.
pub const SCALING_SIZES_SMALL: &[usize] = &[100, 500, 1_000, 2_500, 5_000];

/// Large corpus sizes for thorough scaling tests.
pub const SCALING_SIZES_LARGE: &[usize] = &[10_000, 25_000, 50_000, 100_000, 250_000];

/// Run a scaling benchmark across multiple corpus sizes.
///
/// # Arguments
/// * `c` - The criterion instance
/// * `group_name` - Name for the benchmark group
/// * `sizes` - Slice of corpus sizes to test
/// * `setup` - Function that creates the test data for a given size
/// * `bench` - Function that runs the benchmark on the test data
///
/// # Example
/// ```ignore
/// bench_scaling(
///     &mut criterion,
///     "vector_search",
///     &SCALING_SIZES,
///     |size| create_vector_index(size),
///     |index| index.search(&query, 25),
/// );
/// ```
pub fn bench_scaling<T, S, B>(
    c: &mut Criterion,
    group_name: &str,
    sizes: &[usize],
    mut setup: S,
    mut bench: B,
) where
    S: FnMut(usize) -> T,
    B: FnMut(&T),
{
    let mut group = c.benchmark_group(group_name);
    group.sampling_mode(SamplingMode::Auto);
    group.plot_config(PlotConfiguration::default());

    for &size in sizes {
        let data = setup(size);
        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, _| {
            b.iter(|| bench(&data));
        });
    }

    group.finish();
}

/// Run a scaling benchmark with explicit throughput measurement.
///
/// Similar to `bench_scaling` but allows custom throughput specification.
pub fn bench_scaling_with_throughput<T, S, B>(
    c: &mut Criterion,
    group_name: &str,
    sizes: &[usize],
    throughput_fn: fn(usize) -> Throughput,
    mut setup: S,
    mut bench: B,
) where
    S: FnMut(usize) -> T,
    B: FnMut(&T),
{
    let mut group = c.benchmark_group(group_name);

    for &size in sizes {
        let data = setup(size);
        group.throughput(throughput_fn(size));
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, _| {
            b.iter(|| bench(&data));
        });
    }

    group.finish();
}

/// Run a comparison benchmark between two implementations.
///
/// # Arguments
/// * `c` - The criterion instance
/// * `name` - Benchmark name
/// * `baseline` - The baseline (old) implementation
/// * `optimized` - The optimized (new) implementation
pub fn bench_comparison<F1, F2>(c: &mut Criterion, name: &str, mut baseline: F1, mut optimized: F2)
where
    F1: FnMut(),
    F2: FnMut(),
{
    let mut group = c.benchmark_group(name);
    group.bench_function("baseline", |b| b.iter(&mut baseline));
    group.bench_function("optimized", |b| b.iter(&mut optimized));
    group.finish();
}

/// Parameters for vector index benchmarks.
#[derive(Debug, Clone)]
pub struct VectorBenchParams {
    pub dimension: usize,
    pub corpus_size: usize,
    pub query_count: usize,
    pub top_k: usize,
}

impl Default for VectorBenchParams {
    fn default() -> Self {
        Self {
            dimension: 384, // Standard embedding dimension
            corpus_size: 10_000,
            query_count: 1,
            top_k: 25,
        }
    }
}

impl VectorBenchParams {
    pub fn small() -> Self {
        Self {
            dimension: 64,
            corpus_size: 1_000,
            query_count: 1,
            top_k: 10,
        }
    }

    pub fn medium() -> Self {
        Self {
            dimension: 384,
            corpus_size: 25_000,
            query_count: 1,
            top_k: 25,
        }
    }

    pub fn large() -> Self {
        Self {
            dimension: 384,
            corpus_size: 100_000,
            query_count: 1,
            top_k: 25,
        }
    }

    pub fn with_dimension(mut self, dim: usize) -> Self {
        self.dimension = dim;
        self
    }

    pub fn with_corpus_size(mut self, size: usize) -> Self {
        self.corpus_size = size;
        self
    }

    pub fn with_top_k(mut self, k: usize) -> Self {
        self.top_k = k;
        self
    }
}

/// Parameters for search benchmarks.
#[derive(Debug, Clone)]
pub struct SearchBenchParams {
    pub query_length: usize,
    pub result_limit: usize,
    pub with_filters: bool,
}

impl Default for SearchBenchParams {
    fn default() -> Self {
        Self {
            query_length: 3, // Typical short query
            result_limit: 25,
            with_filters: false,
        }
    }
}

impl SearchBenchParams {
    pub fn with_filters(mut self) -> Self {
        self.with_filters = true;
        self
    }

    pub fn with_limit(mut self, limit: usize) -> Self {
        self.result_limit = limit;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vector_bench_params_default() {
        let params = VectorBenchParams::default();
        assert_eq!(params.dimension, 384);
        assert_eq!(params.corpus_size, 10_000);
        assert_eq!(params.top_k, 25);
    }

    #[test]
    fn test_vector_bench_params_builder() {
        let params = VectorBenchParams::default()
            .with_dimension(128)
            .with_corpus_size(5_000);
        assert_eq!(params.dimension, 128);
        assert_eq!(params.corpus_size, 5_000);
    }

    #[test]
    fn test_search_bench_params() {
        let params = SearchBenchParams::default().with_filters().with_limit(50);
        assert!(params.with_filters);
        assert_eq!(params.result_limit, 50);
    }
}
