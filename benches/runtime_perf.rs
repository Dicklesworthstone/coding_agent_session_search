use coding_agent_search::connectors::{NormalizedConversation, NormalizedMessage};
use coding_agent_search::indexer::persist::persist_conversation;
use coding_agent_search::search::query::SearchClient;
use coding_agent_search::search::tantivy::index_dir;
use coding_agent_search::search::vector_index::{dot_product_scalar_bench, dot_product_simd_bench};
use coding_agent_search::storage::sqlite::SqliteStorage;
use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use std::path::PathBuf;
use tempfile::TempDir;

fn sample_conv(i: i64, msgs: i64) -> NormalizedConversation {
    let mut messages = Vec::new();
    for m in 0..msgs {
        messages.push(NormalizedMessage {
            idx: m,
            role: if m % 2 == 0 { "user" } else { "agent" }.into(),
            author: None,
            created_at: Some(1_700_000_000_000 + (i * 10 + m)),
            content: format!("conversation {i} message {m} lorem ipsum dolor sit amet"),
            extra: serde_json::json!({}),
            snippets: Vec::new(),
        });
    }
    NormalizedConversation {
        agent_slug: "bench-agent".into(),
        external_id: Some(format!("conv-{i}")),
        title: Some(format!("Conversation {i}")),
        workspace: Some(PathBuf::from("/tmp/workspace")),
        source_path: PathBuf::from(format!("/tmp/bench/conv-{i}.jsonl")),
        started_at: Some(1_700_000_000_000),
        ended_at: Some(1_700_000_000_000 + msgs),
        metadata: serde_json::json!({ "bench": true, "i": i }),
        messages,
    }
}

fn seed_index(conv_count: i64, msgs: i64) -> (TempDir, SearchClient) {
    let temp = TempDir::new().expect("tempdir");
    let data_dir = temp.path().to_path_buf();
    let db_path = data_dir.join("bench.db");
    let index_path = index_dir(&data_dir).expect("index path");

    let mut storage = SqliteStorage::open(&db_path).expect("open db");
    let mut t_index =
        coding_agent_search::search::tantivy::TantivyIndex::open_or_create(&index_path).unwrap();

    for i in 0..conv_count {
        let conv = sample_conv(i, msgs);
        persist_conversation(&mut storage, &mut t_index, &conv).expect("persist");
    }
    t_index.commit().unwrap();

    // For perf benches we rely solely on Tantivy (no SQLite fallback) to avoid
    // FTS quirks impacting measurements.
    let client = SearchClient::open(&index_path, None)
        .expect("open client")
        .expect("client available");

    (temp, client)
}

fn bench_indexing(c: &mut Criterion) {
    c.bench_function("index_small_batch", |b| {
        b.iter_batched(
            || {
                let temp = TempDir::new().unwrap();
                let data_dir = temp.path().to_path_buf();
                let db_path = data_dir.join("bench.db");
                let index_path = index_dir(&data_dir).unwrap();
                (
                    temp,
                    SqliteStorage::open(&db_path).unwrap(),
                    coding_agent_search::search::tantivy::TantivyIndex::open_or_create(&index_path)
                        .unwrap(),
                )
            },
            |(temp, mut storage, mut idx)| {
                let _keep = temp; // keep tempdir alive
                for i in 0..10 {
                    let conv = sample_conv(i, 10);
                    persist_conversation(&mut storage, &mut idx, &conv).unwrap();
                }
                idx.commit().unwrap();
            },
            BatchSize::SmallInput,
        );
    });
}

fn bench_search(c: &mut Criterion) {
    let (_tmp, client) = seed_index(40, 12);
    c.bench_function("search_latency", |b| {
        b.iter(|| {
            let hits = client
                .search(
                    black_box("lorem"),
                    coding_agent_search::search::query::SearchFilters::default(),
                    20,
                    0,
                    coding_agent_search::search::query::FieldMask::FULL,
                )
                .unwrap();
            black_box(hits.len());
        })
    });
}

fn bench_dot_product(c: &mut Criterion) {
    let a: Vec<f32> = (0..384).map(|i| i as f32 * 0.001).collect();
    let b: Vec<f32> = (0..384).map(|i| i as f32 * 0.002).collect();

    c.bench_function("dot_product_scalar", |bencher| {
        bencher.iter(|| black_box(dot_product_scalar_bench(black_box(&a), black_box(&b))))
    });
    c.bench_function("dot_product_simd", |bencher| {
        bencher.iter(|| black_box(dot_product_simd_bench(black_box(&a), black_box(&b))))
    });
}

// ============================================================
// Wildcard Performance Benchmarks (bd-d5a)
// ============================================================

/// Sample conversation with varied content for wildcard testing
fn wildcard_sample_conv(i: i64, msgs: i64) -> NormalizedConversation {
    // Use varied vocabulary to test different wildcard patterns
    let word_pool = [
        "function",
        "handler",
        "config",
        "error",
        "request",
        "response",
        "database",
        "connection",
        "authentication",
        "validation",
        "serialize",
        "deserialize",
        "controller",
        "middleware",
        "async",
        "performance",
        "optimization",
        "benchmark",
        "iterator",
        "collection",
    ];

    let mut messages = Vec::new();
    for m in 0..msgs {
        let w1 = word_pool[(i as usize + m as usize) % word_pool.len()];
        let w2 = word_pool[(i as usize + m as usize + 7) % word_pool.len()];
        let w3 = word_pool[(i as usize + m as usize + 13) % word_pool.len()];

        messages.push(NormalizedMessage {
            idx: m,
            role: if m % 2 == 0 { "user" } else { "agent" }.into(),
            author: None,
            created_at: Some(1_700_000_000_000 + (i * 10 + m)),
            content: format!(
                "The {w1} module needs a new {w2}Handler class. \
                 Consider using {w3}Config for better {w1} integration. \
                 Error handling via {w2}Error and {w3}Validator is recommended."
            ),
            extra: serde_json::json!({}),
            snippets: Vec::new(),
        });
    }
    NormalizedConversation {
        agent_slug: "bench-agent".into(),
        external_id: Some(format!("wildcard-conv-{i}")),
        title: Some(format!(
            "Wildcard Test {i}: {}",
            word_pool[i as usize % word_pool.len()]
        )),
        workspace: Some(PathBuf::from("/tmp/workspace")),
        source_path: PathBuf::from(format!("/tmp/bench/wildcard-{i}.jsonl")),
        started_at: Some(1_700_000_000_000),
        ended_at: Some(1_700_000_000_000 + msgs),
        metadata: serde_json::json!({ "bench": true, "wildcard_test": true }),
        messages,
    }
}

/// Seed a larger index optimized for wildcard testing
fn seed_wildcard_index(conv_count: i64, msgs_per_conv: i64) -> (TempDir, SearchClient) {
    let temp = TempDir::new().expect("tempdir");
    let data_dir = temp.path().to_path_buf();
    let db_path = data_dir.join("wildcard_bench.db");
    let index_path = index_dir(&data_dir).expect("index path");

    let mut storage = SqliteStorage::open(&db_path).expect("open db");
    let mut t_index =
        coding_agent_search::search::tantivy::TantivyIndex::open_or_create(&index_path).unwrap();

    for i in 0..conv_count {
        let conv = wildcard_sample_conv(i, msgs_per_conv);
        persist_conversation(&mut storage, &mut t_index, &conv).expect("persist");
    }
    t_index.commit().unwrap();

    let client = SearchClient::open(&index_path, Some(&db_path))
        .expect("open client")
        .expect("client available");

    (temp, client)
}

/// Benchmark exact match (baseline for comparison)
fn bench_wildcard_exact(c: &mut Criterion) {
    // 100 conversations x 20 messages = 2000 documents
    let (_tmp, client) = seed_wildcard_index(100, 20);
    let filters = coding_agent_search::search::query::SearchFilters::default();

    c.bench_function("wildcard_exact_match", |b| {
        b.iter(|| {
            let hits = client
                .search(
                    black_box("handler"),
                    filters.clone(),
                    20,
                    0,
                    coding_agent_search::search::query::FieldMask::FULL,
                )
                .unwrap();
            black_box(hits.len())
        })
    });
}

/// Benchmark prefix wildcard: hand* (uses edge n-grams - should be fast)
fn bench_wildcard_prefix(c: &mut Criterion) {
    let (_tmp, client) = seed_wildcard_index(100, 20);
    let filters = coding_agent_search::search::query::SearchFilters::default();

    c.bench_function("wildcard_prefix_pattern", |b| {
        b.iter(|| {
            let hits = client
                .search(
                    black_box("hand*"),
                    filters.clone(),
                    20,
                    0,
                    coding_agent_search::search::query::FieldMask::FULL,
                )
                .unwrap();
            black_box(hits.len())
        })
    });
}

/// Benchmark suffix wildcard: *ler (uses RegexQuery - potentially slower)
fn bench_wildcard_suffix(c: &mut Criterion) {
    let (_tmp, client) = seed_wildcard_index(100, 20);
    let filters = coding_agent_search::search::query::SearchFilters::default();

    c.bench_function("wildcard_suffix_pattern", |b| {
        b.iter(|| {
            let hits = client
                .search(
                    black_box("*handler"),
                    filters.clone(),
                    20,
                    0,
                    coding_agent_search::search::query::FieldMask::FULL,
                )
                .unwrap();
            black_box(hits.len())
        })
    });
}

/// Benchmark substring wildcard: *andl* (uses RegexQuery - potentially slowest)
fn bench_wildcard_substring(c: &mut Criterion) {
    let (_tmp, client) = seed_wildcard_index(100, 20);
    let filters = coding_agent_search::search::query::SearchFilters::default();

    c.bench_function("wildcard_substring_pattern", |b| {
        b.iter(|| {
            let hits = client
                .search(
                    black_box("*config*"),
                    filters.clone(),
                    20,
                    0,
                    coding_agent_search::search::query::FieldMask::FULL,
                )
                .unwrap();
            black_box(hits.len())
        })
    });
}

/// Benchmark suffix wildcard with common ending: *Error
fn bench_wildcard_suffix_common(c: &mut Criterion) {
    let (_tmp, client) = seed_wildcard_index(100, 20);
    let filters = coding_agent_search::search::query::SearchFilters::default();

    c.bench_function("wildcard_suffix_common", |b| {
        b.iter(|| {
            let hits = client
                .search(
                    black_box("*error"),
                    filters.clone(),
                    20,
                    0,
                    coding_agent_search::search::query::FieldMask::FULL,
                )
                .unwrap();
            black_box(hits.len())
        })
    });
}

/// Benchmark larger dataset (stress test)
fn bench_wildcard_large_dataset(c: &mut Criterion) {
    // 500 conversations x 20 messages = 10000 documents
    let (_tmp, client) = seed_wildcard_index(500, 20);
    let filters = coding_agent_search::search::query::SearchFilters::default();

    let mut group = c.benchmark_group("wildcard_large_dataset");

    group.bench_function("exact", |b| {
        b.iter(|| {
            let hits = client
                .search(
                    black_box("validation"),
                    filters.clone(),
                    20,
                    0,
                    coding_agent_search::search::query::FieldMask::FULL,
                )
                .unwrap();
            black_box(hits.len())
        })
    });

    group.bench_function("prefix", |b| {
        b.iter(|| {
            let hits = client
                .search(
                    black_box("valid*"),
                    filters.clone(),
                    20,
                    0,
                    coding_agent_search::search::query::FieldMask::FULL,
                )
                .unwrap();
            black_box(hits.len())
        })
    });

    group.bench_function("suffix", |b| {
        b.iter(|| {
            let hits = client
                .search(
                    black_box("*tion"),
                    filters.clone(),
                    20,
                    0,
                    coding_agent_search::search::query::FieldMask::FULL,
                )
                .unwrap();
            black_box(hits.len())
        })
    });

    group.bench_function("substring", |b| {
        b.iter(|| {
            let hits = client
                .search(
                    black_box("*valid*"),
                    filters.clone(),
                    20,
                    0,
                    coding_agent_search::search::query::FieldMask::FULL,
                )
                .unwrap();
            black_box(hits.len())
        })
    });

    group.finish();
}

// ============================================================
// Concurrent & Scaling Benchmarks
// ============================================================

/// Benchmark parallel data generation throughput using rayon.
///
/// Note: SearchClient contains rusqlite::Connection which is !Send/!Sync,
/// so we can't share it across threads for concurrent search. Instead we
/// measure parallel data generation which tests rayon integration and
/// throughput of the conversation generation infrastructure.
fn bench_concurrent_indexing(c: &mut Criterion) {
    use rayon::prelude::*;

    let mut group = c.benchmark_group("concurrent_indexing");

    // Benchmark parallel conversation generation (simulates concurrent workload)
    group.bench_function("generate_100_convs_parallel", |b| {
        b.iter(|| {
            let convs: Vec<_> = (0..100i64)
                .into_par_iter()
                .map(|i| sample_conv(i, 10))
                .collect();
            black_box(convs.len())
        })
    });

    // Benchmark sequential for comparison
    group.bench_function("generate_100_convs_sequential", |b| {
        b.iter(|| {
            let convs: Vec<_> = (0..100i64).map(|i| sample_conv(i, 10)).collect();
            black_box(convs.len())
        })
    });

    group.finish();
}

/// Benchmark rapid sequential searches (simulates interactive use)
fn bench_rapid_sequential_search(c: &mut Criterion) {
    let (_tmp, client) = seed_index(200, 15);
    let filters = coding_agent_search::search::query::SearchFilters::default();

    let mut group = c.benchmark_group("rapid_sequential");

    // Simulate rapid user typing - many queries in sequence
    group.bench_function("10_queries_sequential", |b| {
        let queries = [
            "lorem",
            "ipsum",
            "dolor",
            "sit",
            "amet",
            "consectetur",
            "adipiscing",
            "elit",
            "sed",
            "do",
        ];
        b.iter(|| {
            for q in &queries {
                let hits = client
                    .search(
                        black_box(*q),
                        filters.clone(),
                        20,
                        0,
                        coding_agent_search::search::query::FieldMask::FULL,
                    )
                    .unwrap();
                black_box(hits.len());
            }
        })
    });

    // Simulate search refinement - increasingly specific queries
    group.bench_function("refinement_sequence", |b| {
        let queries = ["l", "lo", "lor", "lore", "lorem"];
        b.iter(|| {
            for q in &queries {
                let hits = client
                    .search(
                        black_box(*q),
                        filters.clone(),
                        20,
                        0,
                        coding_agent_search::search::query::FieldMask::FULL,
                    )
                    .unwrap();
                black_box(hits.len());
            }
        })
    });

    group.finish();
}

// ============================================================
// Scaling Benchmarks
// ============================================================

/// Benchmark search latency at different index sizes
fn bench_search_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("search_scaling");
    group.sample_size(20); // Fewer samples for larger datasets

    // Small: 50 conversations
    let (_tmp_small, client_small) = seed_index(50, 10);
    let filters = coding_agent_search::search::query::SearchFilters::default();

    group.bench_function("50_convs", |b| {
        b.iter(|| {
            let hits = client_small
                .search(
                    black_box("lorem"),
                    filters.clone(),
                    20,
                    0,
                    coding_agent_search::search::query::FieldMask::FULL,
                )
                .unwrap();
            black_box(hits.len())
        })
    });

    // Medium: 200 conversations
    let (_tmp_med, client_med) = seed_index(200, 10);
    group.bench_function("200_convs", |b| {
        b.iter(|| {
            let hits = client_med
                .search(
                    black_box("lorem"),
                    filters.clone(),
                    20,
                    0,
                    coding_agent_search::search::query::FieldMask::FULL,
                )
                .unwrap();
            black_box(hits.len())
        })
    });

    // Large: 500 conversations
    let (_tmp_large, client_large) = seed_index(500, 10);
    group.bench_function("500_convs", |b| {
        b.iter(|| {
            let hits = client_large
                .search(
                    black_box("lorem"),
                    filters.clone(),
                    20,
                    0,
                    coding_agent_search::search::query::FieldMask::FULL,
                )
                .unwrap();
            black_box(hits.len())
        })
    });

    group.finish();
}

criterion_group!(
    runtime_perf,
    bench_indexing,
    bench_search,
    bench_dot_product,
    bench_wildcard_exact,
    bench_wildcard_prefix,
    bench_wildcard_suffix,
    bench_wildcard_substring,
    bench_wildcard_suffix_common,
    bench_wildcard_large_dataset,
    bench_concurrent_indexing,
    bench_rapid_sequential_search,
    bench_search_scaling,
);
criterion_main!(runtime_perf);
