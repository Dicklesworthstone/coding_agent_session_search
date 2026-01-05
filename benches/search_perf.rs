use coding_agent_search::default_data_dir;
use coding_agent_search::search::canonicalize::canonicalize_for_embedding;
use coding_agent_search::search::embedder::Embedder;
use coding_agent_search::search::hash_embedder::HashEmbedder;
use coding_agent_search::search::query::{
    rrf_fuse_hits, MatchType, SearchClient, SearchFilters, SearchHit,
};
use coding_agent_search::search::tantivy::index_dir;
use coding_agent_search::search::vector_index::{
    Quantization, SemanticFilter, VectorEntry, VectorIndex,
};
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use std::collections::HashSet;
use std::hint::black_box;

// =============================================================================
// Hash Embedder Benchmarks
// =============================================================================

/// Benchmark hash embedder on 1000 documents.
/// Target: <1ms per doc (so <1s total for 1000 docs)
fn bench_hash_embed_1000_docs(c: &mut Criterion) {
    let embedder = HashEmbedder::default_dimension();
    let docs: Vec<String> = (0..1000)
        .map(|i| format!("This is document number {} with some sample content for embedding benchmarks. It contains various words like rust programming language testing performance.", i))
        .collect();

    c.bench_function("hash_embed_1000_docs", |b| {
        b.iter(|| {
            for doc in &docs {
                let _ = black_box(embedder.embed(doc));
            }
        })
    });
}

/// Benchmark hash embedder batch embedding.
fn bench_hash_embed_batch(c: &mut Criterion) {
    let embedder = HashEmbedder::default_dimension();
    let docs: Vec<&str> = (0..100)
        .map(|_| "Sample document for batch embedding benchmark with multiple words")
        .collect();

    c.bench_function("hash_embed_batch_100", |b| {
        b.iter(|| {
            let _ = black_box(embedder.embed_batch(&docs));
        })
    });
}

// =============================================================================
// Canonicalization Benchmarks
// =============================================================================

/// Benchmark canonicalization of a long message.
fn bench_canonicalize_long_message(c: &mut Criterion) {
    // Create a realistic long message (~10KB)
    let long_message: String = (0..100)
        .map(|i| {
            format!(
                "Paragraph {}: Lorem ipsum dolor sit amet, consectetur adipiscing elit. \
                 Sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. \
                 Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris. ",
                i
            )
        })
        .collect();

    c.bench_function("canonicalize_long_message", |b| {
        b.iter(|| black_box(canonicalize_for_embedding(&long_message)))
    });
}

/// Benchmark canonicalization with code blocks.
fn bench_canonicalize_with_code(c: &mut Criterion) {
    let message_with_code = r#"
Here's the Rust code to implement a binary search:

```rust
fn binary_search<T: Ord>(arr: &[T], target: &T) -> Option<usize> {
    let mut left = 0;
    let mut right = arr.len();

    while left < right {
        let mid = left + (right - left) / 2;
        match arr[mid].cmp(target) {
            std::cmp::Ordering::Equal => return Some(mid),
            std::cmp::Ordering::Less => left = mid + 1,
            std::cmp::Ordering::Greater => right = mid,
        }
    }
    None
}
```

This has O(log n) time complexity and O(1) space complexity.
"#;

    c.bench_function("canonicalize_with_code", |b| {
        b.iter(|| black_box(canonicalize_for_embedding(message_with_code)))
    });
}

// =============================================================================
// RRF Fusion Benchmarks
// =============================================================================

/// Create a test search hit for benchmarking.
fn make_bench_hit(id: &str, score: f32) -> SearchHit {
    SearchHit {
        title: id.to_string(),
        snippet: format!("Snippet for {id}"),
        content: format!("Content for {id}"),
        score,
        source_path: format!("/path/to/{id}.jsonl"),
        agent: "test".to_string(),
        workspace: "/workspace".to_string(),
        workspace_original: None,
        created_at: Some(1704067200000), // 2024-01-01
        line_number: Some(1),
        match_type: MatchType::Exact,
        source_id: "local".to_string(),
        origin_kind: "local".to_string(),
        origin_host: None,
    }
}

/// Benchmark RRF fusion with 100 results from each source.
/// Target: <5ms
fn bench_rrf_fusion_100_results(c: &mut Criterion) {
    let lexical: Vec<SearchHit> = (0..100)
        .map(|i| make_bench_hit(&format!("L{i}"), 100.0 - i as f32))
        .collect();

    let semantic: Vec<SearchHit> = (0..100)
        .map(|i| make_bench_hit(&format!("S{i}"), 1.0 - 0.01 * i as f32))
        .collect();

    c.bench_function("rrf_fusion_100_results", |b| {
        b.iter(|| {
            let fused = rrf_fuse_hits(black_box(&lexical), black_box(&semantic), 25, 0);
            black_box(fused)
        })
    });
}

/// Benchmark RRF fusion with overlapping results.
fn bench_rrf_fusion_overlapping(c: &mut Criterion) {
    // 50% overlap between lexical and semantic
    let lexical: Vec<SearchHit> = (0..100)
        .map(|i| make_bench_hit(&format!("doc{i}"), 100.0 - i as f32))
        .collect();

    let semantic: Vec<SearchHit> = (50..150)
        .map(|i| make_bench_hit(&format!("doc{i}"), 1.0 - 0.01 * (i - 50) as f32))
        .collect();

    c.bench_function("rrf_fusion_50pct_overlap", |b| {
        b.iter(|| {
            let fused = rrf_fuse_hits(black_box(&lexical), black_box(&semantic), 25, 0);
            black_box(fused)
        })
    });
}

// =============================================================================
// Vector Index Benchmarks
// =============================================================================

fn bench_empty_search(c: &mut Criterion) {
    let data_dir = default_data_dir();
    let index_path = index_dir(&data_dir).unwrap();
    let client = SearchClient::open(&index_path, None).unwrap();
    if let Some(client) = client {
        c.bench_function("search_empty_query", |b| {
            b.iter(|| {
                client
                    .search("", SearchFilters::default(), 10, 0)
                    .unwrap_or_default()
            })
        });
    }
}

/// Benchmark vector search with 10k entries.
/// Target: <5ms
fn bench_vector_index_search_10k(c: &mut Criterion) {
    let dimension = 384;
    let count = 10_000;
    let entries = build_entries(count, dimension);
    let index = VectorIndex::build(
        "bench-embedder",
        "rev",
        dimension,
        Quantization::F16,
        entries,
    )
    .unwrap();
    let query = build_query(dimension);

    c.bench_function("vector_index_search_10k", |b| {
        b.iter(|| {
            let results = index
                .search_top_k(black_box(&query), 25, None)
                .unwrap_or_default();
            black_box(results);
        });
    });
}

/// Benchmark vector search with 50k entries (no filter).
/// Target: <20ms
fn bench_vector_index_search_50k(c: &mut Criterion) {
    let dimension = 384;
    let count = 50_000;
    let entries = build_entries(count, dimension);
    let index = VectorIndex::build(
        "bench-embedder",
        "rev",
        dimension,
        Quantization::F16,
        entries,
    )
    .unwrap();
    let query = build_query(dimension);

    c.bench_function("vector_index_search_50k", |b| {
        b.iter(|| {
            let results = index
                .search_top_k(black_box(&query), 25, None)
                .unwrap_or_default();
            black_box(results);
        });
    });
}

/// Benchmark vector search with 50k entries and filtering.
/// Target: <20ms
fn bench_vector_index_search_50k_filtered(c: &mut Criterion) {
    let dimension = 384;
    let count = 50_000;
    let entries = build_entries(count, dimension);
    let index = VectorIndex::build(
        "bench-embedder",
        "rev",
        dimension,
        Quantization::F16,
        entries,
    )
    .unwrap();
    let query = build_query(dimension);

    // Filter to agents 0, 1, 2 (out of 8 possible)
    let mut agent_filter = HashSet::new();
    agent_filter.insert(0u32);
    agent_filter.insert(1u32);
    agent_filter.insert(2u32);

    let filter = SemanticFilter {
        agents: Some(agent_filter),
        workspaces: None,
        sources: None,
        roles: None,
        created_from: None,
        created_to: None,
    };

    c.bench_function("vector_index_search_50k_filtered", |b| {
        b.iter(|| {
            let results = index
                .search_top_k(black_box(&query), 25, Some(&filter))
                .unwrap_or_default();
            black_box(results);
        });
    });
}

/// Parameterized benchmark for different index sizes.
fn bench_vector_search_scaling(c: &mut Criterion) {
    let dimension = 384;
    let mut group = c.benchmark_group("vector_search_scaling");

    for size in [1_000, 5_000, 10_000, 25_000, 50_000] {
        let entries = build_entries(size, dimension);
        let index = VectorIndex::build(
            "bench-embedder",
            "rev",
            dimension,
            Quantization::F16,
            entries,
        )
        .unwrap();
        let query = build_query(dimension);

        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, _| {
            b.iter(|| {
                let results = index
                    .search_top_k(black_box(&query), 25, None)
                    .unwrap_or_default();
                black_box(results);
            });
        });
    }
    group.finish();
}

fn build_entries(count: usize, dimension: usize) -> Vec<VectorEntry> {
    let mut entries = Vec::with_capacity(count);
    for idx in 0..count {
        let mut vector = Vec::with_capacity(dimension);
        for d in 0..dimension {
            let value = ((idx + d * 31) % 997) as f32 / 997.0;
            vector.push(value);
        }
        entries.push(VectorEntry {
            message_id: idx as u64,
            created_at_ms: idx as i64,
            agent_id: (idx % 8) as u32,
            workspace_id: 1,
            source_id: 1,
            role: 1,
            chunk_idx: 0,
            content_hash: [0u8; 32],
            vector,
        });
    }
    entries
}

fn build_query(dimension: usize) -> Vec<f32> {
    let mut query = Vec::with_capacity(dimension);
    for d in 0..dimension {
        query.push((d % 17) as f32 / 17.0);
    }
    query
}

criterion_group!(
    benches,
    // Hash embedder benchmarks
    bench_hash_embed_1000_docs,
    bench_hash_embed_batch,
    // Canonicalization benchmarks
    bench_canonicalize_long_message,
    bench_canonicalize_with_code,
    // RRF fusion benchmarks
    bench_rrf_fusion_100_results,
    bench_rrf_fusion_overlapping,
    // Vector index benchmarks
    bench_empty_search,
    bench_vector_index_search_10k,
    bench_vector_index_search_50k,
    bench_vector_index_search_50k_filtered,
    bench_vector_search_scaling,
);
criterion_main!(benches);
