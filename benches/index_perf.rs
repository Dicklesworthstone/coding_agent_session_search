//! Indexing Performance Benchmarks
//!
//! This module benchmarks indexing performance, including streaming vs batch mode
//! comparisons added in Opt 8.4 (coding_agent_session_search-nkc9).
//!
//! ## Memory Profiling
//!
//! For memory profiling (Peak RSS, memory timeline), use external tools:
//!
//! ### Peak RSS Comparison
//! ```bash
//! # Batch mode
//! CASS_STREAMING_INDEX=0 /usr/bin/time -v cargo run --release -- index --full 2>&1 | grep "Maximum resident"
//!
//! # Streaming mode (default)
//! /usr/bin/time -v cargo run --release -- index --full 2>&1 | grep "Maximum resident"
//! ```
//!
//! ### Memory Timeline (heaptrack)
//! ```bash
//! # Install heaptrack: apt install heaptrack heaptrack-gui
//! CASS_STREAMING_INDEX=0 heaptrack cargo run --release -- index --full
//! heaptrack_gui heaptrack.*.zst
//!
//! CASS_STREAMING_INDEX=1 heaptrack cargo run --release -- index --full
//! heaptrack_gui heaptrack.*.zst
//! ```
//!
//! ### Memory Timeline (valgrind massif)
//! ```bash
//! CASS_STREAMING_INDEX=0 valgrind --tool=massif cargo run --release -- index --full
//! ms_print massif.out.* > batch_memory.txt
//!
//! CASS_STREAMING_INDEX=1 valgrind --tool=massif cargo run --release -- index --full
//! ms_print massif.out.* > streaming_memory.txt
//! ```
//!
//! ## Expected Results
//! - Peak RSS: 295 MB (batch) → ~150 MB (streaming), ~50% reduction
//! - Throughput: No more than 10% regression
//! - Memory timeline: Streaming should show flat profile vs batch's spike

use coding_agent_search::indexer::{IndexOptions, run_index};
use coding_agent_search::search::tantivy::index_dir;
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use std::fs;
use std::io::Write;
use tempfile::TempDir;

/// Create a test corpus with the specified number of conversations.
///
/// Each conversation has 2 messages (user + assistant).
fn create_corpus(tmp: &TempDir, count: usize) -> (std::path::PathBuf, std::path::PathBuf) {
    let data_dir = tmp.path().join("data");
    let db_path = data_dir.join("agent_search.db");

    // Create Codex-format sessions
    let codex_home = data_dir.clone();
    for i in 0..count {
        let date_path = format!("sessions/2024/11/{:02}", (i % 30) + 1);
        let sessions = codex_home.join(&date_path);
        fs::create_dir_all(&sessions).unwrap();

        let filename = format!("rollout-{i}.jsonl");
        let file = sessions.join(&filename);
        let ts = 1732118400000 + (i as u64 * 1000);
        let content = format!(
            r#"{{"type": "event_msg", "timestamp": {ts}, "payload": {{"type": "user_message", "message": "test message {i} with unique content"}}}}
{{"type": "response_item", "timestamp": {}, "payload": {{"role": "assistant", "content": "response to message {i}"}}}}
"#,
            ts + 1000
        );
        fs::write(file, content).unwrap();
    }

    (data_dir, db_path)
}

fn bench_index_full(c: &mut Criterion) {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let db_path = data_dir.join("agent_search.db");
    let sample_dir = data_dir.join("sample_logs");
    fs::create_dir_all(&sample_dir).unwrap();
    let mut f = fs::File::create(sample_dir.join("rollout-1.jsonl")).unwrap();
    writeln!(f, "{{\"role\":\"user\",\"content\":\"hello\"}}").unwrap();
    writeln!(f, "{{\"role\":\"assistant\",\"content\":\"world\"}}").unwrap();

    let opts = IndexOptions {
        full: true,
        force_rebuild: true,
        watch: false,
        watch_once_paths: None,
        db_path,
        data_dir: data_dir.clone(),
        semantic: false,
        build_hnsw: false,
        embedder: "fastembed".to_string(),
        progress: None,
        watch_interval_secs: 30,
    };

    // create empty index dir so Tantivy opens cleanly
    let _ = index_dir(&data_dir);

    c.bench_function("index_full_empty", |b| {
        b.iter(|| run_index(opts.clone(), None))
    });
}

/// Benchmark streaming vs batch indexing throughput.
///
/// This compares the performance of the streaming indexing mode (Opt 8.2)
/// against the original batch mode. Streaming uses bounded channels with
/// backpressure to reduce peak memory usage.
fn bench_streaming_vs_batch(c: &mut Criterion) {
    let mut group = c.benchmark_group("streaming_vs_batch");

    // Test with multiple corpus sizes to see scaling behavior
    for &corpus_size in &[50, 100, 250] {
        // Create fresh corpus for each size
        let tmp = TempDir::new().unwrap();
        let (data_dir, db_path) = create_corpus(&tmp, corpus_size);

        // Ensure index directory exists
        let _ = index_dir(&data_dir);

        let base_opts = IndexOptions {
            full: true,
            force_rebuild: true,
            watch: false,
            watch_once_paths: None,
            db_path: db_path.clone(),
            data_dir: data_dir.clone(),
            semantic: false,
            build_hnsw: false,
            embedder: "fastembed".to_string(),
            progress: None,
            watch_interval_secs: 30,
        };

        // Benchmark batch mode
        group.bench_with_input(
            BenchmarkId::new("batch", corpus_size),
            &corpus_size,
            |b, _| {
                // Disable streaming for batch mode
                // SAFETY: Benchmarks run single-threaded per test, no concurrent env access
                unsafe { std::env::set_var("CASS_STREAMING_INDEX", "0") };
                let opts = base_opts.clone();
                b.iter(|| {
                    // Clear any existing data for clean measurement
                    let _ = fs::remove_file(&opts.db_path);
                    let _ = fs::remove_dir_all(opts.data_dir.join("index"));
                    run_index(opts.clone(), None)
                });
            },
        );

        // Benchmark streaming mode
        group.bench_with_input(
            BenchmarkId::new("streaming", corpus_size),
            &corpus_size,
            |b, _| {
                // Enable streaming (default)
                // SAFETY: Benchmarks run single-threaded per test, no concurrent env access
                unsafe { std::env::set_var("CASS_STREAMING_INDEX", "1") };
                let opts = base_opts.clone();
                b.iter(|| {
                    // Clear any existing data for clean measurement
                    let _ = fs::remove_file(&opts.db_path);
                    let _ = fs::remove_dir_all(opts.data_dir.join("index"));
                    run_index(opts.clone(), None)
                });
            },
        );
    }

    // Reset to default
    // SAFETY: Benchmarks run single-threaded per test, no concurrent env access
    unsafe { std::env::remove_var("CASS_STREAMING_INDEX") };
    group.finish();
}

/// Benchmark channel overhead in streaming mode.
///
/// Measures the impact of different channel buffer sizes on throughput.
/// The STREAMING_CHANNEL_SIZE constant (32) balances memory vs throughput.
fn bench_channel_overhead(c: &mut Criterion) {
    let corpus_size = 100;
    let tmp = TempDir::new().unwrap();
    let (data_dir, db_path) = create_corpus(&tmp, corpus_size);
    let _ = index_dir(&data_dir);

    let opts = IndexOptions {
        full: true,
        force_rebuild: true,
        watch: false,
        watch_once_paths: None,
        db_path,
        data_dir: data_dir.clone(),
        semantic: false,
        build_hnsw: false,
        embedder: "fastembed".to_string(),
        progress: None,
        watch_interval_secs: 30,
    };

    // Enable streaming mode for this benchmark
    // SAFETY: Benchmarks run single-threaded per test, no concurrent env access
    unsafe { std::env::set_var("CASS_STREAMING_INDEX", "1") };

    c.bench_function("streaming_channel_default", |b| {
        b.iter(|| {
            let opts = opts.clone();
            let _ = fs::remove_file(&opts.db_path);
            let _ = fs::remove_dir_all(opts.data_dir.join("index"));
            run_index(opts, None)
        });
    });

    // SAFETY: Benchmarks run single-threaded per test, no concurrent env access
    unsafe { std::env::remove_var("CASS_STREAMING_INDEX") };
}

criterion_group!(
    benches,
    bench_index_full,
    bench_streaming_vs_batch,
    bench_channel_overhead
);
criterion_main!(benches);
