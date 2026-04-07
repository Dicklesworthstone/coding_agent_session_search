use coding_agent_search::default_data_dir;
use coding_agent_search::search::canonicalize::{MAX_EMBED_CHARS, canonicalize_for_embedding};
use coding_agent_search::search::embedder::Embedder;
use coding_agent_search::search::hash_embedder::HashEmbedder;
use coding_agent_search::search::query::{
    FieldMask, MatchType, SearchClient, SearchFilters, SearchHit, rrf_fuse_hits,
};
use coding_agent_search::search::tantivy::index_dir;
use coding_agent_search::search::vector_index::{
    Quantization, SemanticDocId, SemanticFilter, VectorIndex, dot_product_f16_scalar_bench,
    dot_product_f16_simd_bench, dot_product_scalar_bench, dot_product_simd_bench,
};
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use half::f16;
use std::collections::HashSet;
use std::hint::black_box;
use tempfile::TempDir;

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
                let _ = black_box(embedder.embed_sync(doc));
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
            let _ = black_box(embedder.embed_batch_sync(&docs));
        })
    });
}

// =============================================================================
// Canonicalization Benchmarks
// =============================================================================

/// Benchmark canonicalization of a long message.
fn make_long_message() -> String {
    // Create a realistic long message (~10KB)
    (0..100)
        .map(|i| {
            format!(
                "Paragraph {}: Lorem ipsum dolor sit amet, consectetur adipiscing elit. \
                 Sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. \
                 Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris. ",
                i
            )
        })
        .collect()
}

fn make_sized_message(target_len: usize) -> String {
    let chunk = "This is a sample sentence for canonicalization benchmarks. ";
    let mut msg = String::with_capacity(target_len + chunk.len());
    while msg.len() < target_len {
        msg.push_str(chunk);
    }
    msg.truncate(target_len);
    msg
}

fn bench_canonicalize_long_message(c: &mut Criterion) {
    let long_message = make_long_message();
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

/// Benchmark canonicalization across input sizes.
fn bench_canonicalize_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("canonicalize_scaling");
    let sizes = [100usize, 1_000, 10_000, MAX_EMBED_CHARS + 500];

    for size in sizes {
        let text = make_sized_message(size);
        group.bench_with_input(BenchmarkId::new("canonicalize", size), &text, |b, input| {
            b.iter(|| black_box(canonicalize_for_embedding(input)))
        });
    }
    group.finish();
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
        content_hash: 0,
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
        conversation_id: None,
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
            let fused = rrf_fuse_hits(black_box(&lexical), black_box(&semantic), "", 25, 0);
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
            let fused = rrf_fuse_hits(black_box(&lexical), black_box(&semantic), "", 25, 0);
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
    // Note: This benchmark requires a real index to exist; skipped if not present
    if let Some(client) = client {
        c.bench_function("search_empty_query", |b| {
            b.iter(|| {
                let result = client
                    .search("", SearchFilters::default(), 10, 0, FieldMask::FULL)
                    .unwrap_or_default();
                black_box(result)
            })
        });
    }
}

/// Benchmark vector search with 10k entries.
/// Target: <5ms
fn bench_vector_index_search_10k(c: &mut Criterion) {
    let dimension = 384;
    let count = 10_000;
    let (_tmp, index) =
        build_temp_fsvi_index("bench-embedder", dimension, Quantization::F16, count);
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
    let (_tmp, index) =
        build_temp_fsvi_index("bench-embedder", dimension, Quantization::F16, count);
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
    let (_tmp, index) =
        build_temp_fsvi_index("bench-embedder", dimension, Quantization::F16, count);
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
        let (_tmp, index) =
            build_temp_fsvi_index("bench-embedder", dimension, Quantization::F16, size);
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

fn build_temp_fsvi_index(
    embedder_id: &str,
    dimension: usize,
    quantization: Quantization,
    count: usize,
) -> (TempDir, VectorIndex) {
    let temp = TempDir::new().expect("tempdir");
    let path = temp.path().join("bench.fsvi");
    let mut writer =
        VectorIndex::create_with_revision(&path, embedder_id, "bench", dimension, quantization)
            .expect("create fsvi writer");

    let mut vec_buf = vec![0.0f32; dimension];
    for idx in 0..count {
        for (d, slot) in vec_buf.iter_mut().enumerate() {
            *slot = ((idx + d * 31) % 997) as f32 / 997.0;
        }
        normalize_in_place(&mut vec_buf);

        let doc_id = SemanticDocId {
            message_id: idx as u64,
            chunk_idx: 0,
            agent_id: (idx % 8) as u32,
            workspace_id: 1,
            source_id: 1,
            role: 1,
            created_at_ms: idx as i64,
            content_hash: None,
        }
        .to_doc_id_string();

        writer
            .write_record(&doc_id, &vec_buf)
            .expect("write_record");
    }
    writer.finish().expect("finish fsvi");

    let index = VectorIndex::open(&path).expect("open fsvi");
    (temp, index)
}

fn normalize_in_place(vec: &mut [f32]) {
    let norm_sq: f32 = vec.iter().map(|v| v * v).sum();
    let norm = norm_sq.sqrt();
    if norm > 0.0 {
        for v in vec {
            *v /= norm;
        }
    }
}

fn build_query(dimension: usize) -> Vec<f32> {
    let mut query = Vec::with_capacity(dimension);
    for d in 0..dimension {
        query.push((d % 17) as f32 / 17.0);
    }
    normalize_in_place(&mut query);
    query
}

/// Benchmark vector search with 50k entries loaded from disk (F16 pre-conversion).
/// This tests P0 Opt 1: Pre-Convert F16→F32 Slab at Load Time.
/// Target (local, 2026-01-11): ~1.8ms with pre-conversion, ~4.6ms without.
fn bench_vector_index_search_50k_loaded(c: &mut Criterion) {
    let dimension = 384;
    let count = 50_000;
    let (temp, loaded) =
        build_temp_fsvi_index("bench-embedder", dimension, Quantization::F16, count);
    let query = build_query(dimension);

    c.bench_function("vector_index_search_50k_loaded", |b| {
        b.iter(|| {
            let results = loaded
                .search_top_k(black_box(&query), 25, None)
                .unwrap_or_default();
            black_box(results);
        });
    });
    drop(temp);
}

// =============================================================================
// Opt 1.1: F16 SIMD Dot Product Benchmarks
// =============================================================================

/// Benchmark f32 dot product (scalar vs SIMD) at typical embedding dimensions.
fn bench_dot_product_f32(c: &mut Criterion) {
    let mut group = c.benchmark_group("dot_product_f32");

    for dim in [128, 256, 384, 512, 768, 1024] {
        let a: Vec<f32> = (0..dim).map(|i| (i as f32 * 0.001).sin()).collect();
        let b: Vec<f32> = (0..dim).map(|i| (i as f32 * 0.001).cos()).collect();

        group.bench_with_input(BenchmarkId::new("scalar", dim), &dim, |bench, _| {
            bench.iter(|| black_box(dot_product_scalar_bench(&a, &b)))
        });

        group.bench_with_input(BenchmarkId::new("simd", dim), &dim, |bench, _| {
            bench.iter(|| black_box(dot_product_simd_bench(&a, &b)))
        });
    }
    group.finish();
}

/// Benchmark f16 dot product (scalar vs SIMD) at typical embedding dimensions.
/// Opt 1.1: This measures the impact of the SIMD optimization for f16→f32 dot product.
fn bench_dot_product_f16(c: &mut Criterion) {
    let mut group = c.benchmark_group("dot_product_f16");

    for dim in [128, 256, 384, 512, 768, 1024] {
        let a: Vec<f16> = (0..dim)
            .map(|i| f16::from_f32((i as f32 * 0.001).sin()))
            .collect();
        let b: Vec<f32> = (0..dim).map(|i| (i as f32 * 0.001).cos()).collect();

        group.bench_with_input(BenchmarkId::new("scalar", dim), &dim, |bench, _| {
            bench.iter(|| black_box(dot_product_f16_scalar_bench(&a, &b)))
        });

        group.bench_with_input(BenchmarkId::new("simd", dim), &dim, |bench, _| {
            bench.iter(|| black_box(dot_product_f16_simd_bench(&a, &b)))
        });
    }
    group.finish();
}

/// Benchmark f16 dot product throughput for vector search simulation.
/// Simulates searching through 10k, 25k, 50k vectors at dimension 384.
fn bench_dot_product_f16_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("dot_product_f16_throughput");
    let dim = 384;

    for count in [10_000, 25_000, 50_000] {
        let vectors: Vec<Vec<f16>> = (0..count)
            .map(|i| {
                (0..dim)
                    .map(|d| f16::from_f32(((i + d * 31) % 997) as f32 / 997.0))
                    .collect()
            })
            .collect();
        let query: Vec<f32> = (0..dim).map(|d| (d % 17) as f32 / 17.0).collect();

        group.bench_with_input(BenchmarkId::new("scalar", count), &count, |bench, _| {
            bench.iter(|| {
                let mut sum = 0.0f32;
                for v in &vectors {
                    sum += dot_product_f16_scalar_bench(v, &query);
                }
                black_box(sum)
            })
        });

        group.bench_with_input(BenchmarkId::new("simd", count), &count, |bench, _| {
            bench.iter(|| {
                let mut sum = 0.0f32;
                for v in &vectors {
                    sum += dot_product_f16_simd_bench(v, &query);
                }
                black_box(sum)
            })
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    // Hash embedder benchmarks
    bench_hash_embed_1000_docs,
    bench_hash_embed_batch,
    // Canonicalization benchmarks
    bench_canonicalize_long_message,
    bench_canonicalize_with_code,
    bench_canonicalize_scaling,
    // RRF fusion benchmarks
    bench_rrf_fusion_100_results,
    bench_rrf_fusion_overlapping,
    // Vector index benchmarks
    bench_empty_search,
    bench_vector_index_search_10k,
    bench_vector_index_search_50k,
    bench_vector_index_search_50k_filtered,
    bench_vector_index_search_50k_loaded,
    bench_vector_search_scaling,
    // Opt 1.1: Dot product benchmarks (scalar vs SIMD)
    bench_dot_product_f32,
    bench_dot_product_f16,
    bench_dot_product_f16_throughput,
);
criterion_main!(benches);
