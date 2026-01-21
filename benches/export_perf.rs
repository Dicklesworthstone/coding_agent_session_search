//! Export and compression performance benchmarks for cass.
//!
//! Benchmarks for:
//! - DEFLATE compression at various levels
//! - Chunked data processing
//! - Export + compression + encryption pipeline
//! - Large payload serialization
//!
//! Run with:
//!   cargo bench --bench export_perf
//!
//! Performance targets:
//! | Operation | Target | Size |
//! |-----------|--------|------|
//! | Compress 10MB | < 1s | Level 6 |
//! | Decompress 10MB | < 500ms | Any |
//! | Chunked process 10MB | < 1s | 256KB chunks |
//! | Full pipeline 10MB | < 2s | With encryption |

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use flate2::Compression;
use rand::Rng;
use std::hint::black_box;
use std::io::{Read, Write};

// =============================================================================
// Test Data Generation
// =============================================================================

/// Generate compressible test data (realistic JSON-like content).
fn generate_compressible_data(size: usize) -> Vec<u8> {
    let pattern = r#"{"conversation_id":12345,"message":{"role":"user","content":"This is a sample message with repetitive content for testing compression. Lorem ipsum dolor sit amet, consectetur adipiscing elit.","timestamp":1700000000000},"metadata":{"agent":"test","workspace":"/home/user/project"}}"#;

    let mut data = Vec::with_capacity(size);
    while data.len() < size {
        data.extend_from_slice(pattern.as_bytes());
        data.push(b'\n');
    }
    data.truncate(size);
    data
}

/// Generate random (incompressible) test data.
fn generate_random_data(size: usize) -> Vec<u8> {
    let mut rng = rand::thread_rng();
    let mut data = vec![0u8; size];
    rng.fill(&mut data[..]);
    data
}

/// Generate mixed test data (some compressible, some random).
fn generate_mixed_data(size: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(size);
    let mut rng = rand::thread_rng();

    // Alternate between compressible and random blocks
    let block_size = 4096;
    while data.len() < size {
        if (data.len() / block_size) % 2 == 0 {
            // Compressible block (text)
            let text = "Lorem ipsum dolor sit amet, consectetur adipiscing elit. ".repeat(100);
            let remaining = (size - data.len()).min(text.len());
            data.extend_from_slice(&text.as_bytes()[..remaining]);
        } else {
            // Random block
            let remaining = (size - data.len()).min(block_size);
            let mut random = vec![0u8; remaining];
            rng.fill(&mut random[..]);
            data.extend(random);
        }
    }
    data.truncate(size);
    data
}

// =============================================================================
// Compression Benchmarks
// =============================================================================

/// Benchmark DEFLATE compression at various levels.
fn bench_compress_levels(c: &mut Criterion) {
    let data = generate_compressible_data(1024 * 1024); // 1MB

    let mut group = c.benchmark_group("compress_levels");
    group.throughput(Throughput::Bytes(data.len() as u64));

    for level in [1u32, 6, 9] {
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("level_{}", level)),
            &level,
            |b, &level| {
                b.iter(|| {
                    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::new(level));
                    encoder.write_all(&data).expect("write");
                    let compressed = encoder.finish().expect("finish");
                    black_box(compressed)
                })
            },
        );
    }

    group.finish();
}

/// Benchmark compression with varying data sizes.
fn bench_compress_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("compress_scaling");

    for &size in &[64 * 1024usize, 256 * 1024, 1024 * 1024, 4 * 1024 * 1024] {
        let data = generate_compressible_data(size);

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format_size(size)),
            &size,
            |b, _| {
                b.iter(|| {
                    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::new(6));
                    encoder.write_all(&data).expect("write");
                    let compressed = encoder.finish().expect("finish");
                    black_box(compressed)
                })
            },
        );
    }

    group.finish();
}

/// Benchmark decompression performance.
fn bench_decompress(c: &mut Criterion) {
    let mut group = c.benchmark_group("decompress");

    for &size in &[64 * 1024usize, 256 * 1024, 1024 * 1024, 4 * 1024 * 1024] {
        let original = generate_compressible_data(size);

        // Pre-compress the data
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::new(6));
        encoder.write_all(&original).expect("write");
        let compressed = encoder.finish().expect("finish");

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format_size(size)),
            &compressed,
            |b, compressed| {
                b.iter(|| {
                    let mut decoder = DeflateDecoder::new(&compressed[..]);
                    let mut decompressed = Vec::with_capacity(size);
                    decoder.read_to_end(&mut decompressed).expect("decompress");
                    black_box(decompressed)
                })
            },
        );
    }

    group.finish();
}

/// Benchmark compression with different data types.
fn bench_compress_data_types(c: &mut Criterion) {
    let size = 1024 * 1024; // 1MB

    let compressible = generate_compressible_data(size);
    let random = generate_random_data(size);
    let mixed = generate_mixed_data(size);

    let mut group = c.benchmark_group("compress_data_types");
    group.throughput(Throughput::Bytes(size as u64));

    group.bench_function("compressible", |b| {
        b.iter(|| {
            let mut encoder = DeflateEncoder::new(Vec::new(), Compression::new(6));
            encoder.write_all(&compressible).expect("write");
            let compressed = encoder.finish().expect("finish");
            black_box(compressed)
        })
    });

    group.bench_function("random", |b| {
        b.iter(|| {
            let mut encoder = DeflateEncoder::new(Vec::new(), Compression::new(6));
            encoder.write_all(&random).expect("write");
            let compressed = encoder.finish().expect("finish");
            black_box(compressed)
        })
    });

    group.bench_function("mixed", |b| {
        b.iter(|| {
            let mut encoder = DeflateEncoder::new(Vec::new(), Compression::new(6));
            encoder.write_all(&mixed).expect("write");
            let compressed = encoder.finish().expect("finish");
            black_box(compressed)
        })
    });

    group.finish();
}

// =============================================================================
// Chunked Processing Benchmarks
// =============================================================================

/// Benchmark chunked compression of large data.
fn bench_chunked_compress(c: &mut Criterion) {
    let total_size = 10 * 1024 * 1024; // 10MB
    let data = generate_compressible_data(total_size);

    let mut group = c.benchmark_group("chunked_compress");
    group.throughput(Throughput::Bytes(total_size as u64));
    group.sample_size(20);

    for &chunk_size in &[64 * 1024usize, 256 * 1024, 1024 * 1024, 8 * 1024 * 1024] {
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}_chunks", format_size(chunk_size))),
            &chunk_size,
            |b, &chunk_size| {
                b.iter(|| {
                    let mut compressed_chunks = Vec::new();
                    for chunk in data.chunks(chunk_size) {
                        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::new(6));
                        encoder.write_all(chunk).expect("write");
                        let compressed = encoder.finish().expect("finish");
                        compressed_chunks.push(compressed);
                    }
                    black_box(compressed_chunks)
                })
            },
        );
    }

    group.finish();
}

/// Benchmark streaming compression (incremental writes).
fn bench_streaming_compress(c: &mut Criterion) {
    let total_size = 4 * 1024 * 1024; // 4MB
    let data = generate_compressible_data(total_size);

    let mut group = c.benchmark_group("streaming_compress");
    group.throughput(Throughput::Bytes(total_size as u64));

    // Test different write buffer sizes
    for &write_size in &[4096usize, 16384, 65536] {
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}_writes", format_size(write_size))),
            &write_size,
            |b, &write_size| {
                b.iter(|| {
                    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::new(6));
                    for chunk in data.chunks(write_size) {
                        encoder.write_all(chunk).expect("write");
                    }
                    let compressed = encoder.finish().expect("finish");
                    black_box(compressed)
                })
            },
        );
    }

    group.finish();
}

// =============================================================================
// Roundtrip Benchmarks
// =============================================================================

/// Benchmark compress + decompress roundtrip.
fn bench_roundtrip(c: &mut Criterion) {
    let mut group = c.benchmark_group("compress_roundtrip");

    for &size in &[256 * 1024usize, 1024 * 1024, 4 * 1024 * 1024] {
        let original = generate_compressible_data(size);

        group.throughput(Throughput::Bytes(size as u64 * 2)); // compress + decompress
        group.bench_with_input(
            BenchmarkId::from_parameter(format_size(size)),
            &size,
            |b, _| {
                b.iter(|| {
                    // Compress
                    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::new(6));
                    encoder.write_all(&original).expect("write");
                    let compressed = encoder.finish().expect("finish");

                    // Decompress
                    let mut decoder = DeflateDecoder::new(&compressed[..]);
                    let mut decompressed = Vec::with_capacity(size);
                    decoder.read_to_end(&mut decompressed).expect("decompress");

                    black_box(decompressed)
                })
            },
        );
    }

    group.finish();
}

// =============================================================================
// Serialization Benchmarks
// =============================================================================

/// Benchmark JSON serialization of conversation-like structures.
fn bench_json_serialize(c: &mut Criterion) {
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize)]
    struct TestMessage {
        idx: i64,
        role: String,
        content: String,
        created_at: Option<i64>,
    }

    #[derive(Serialize, Deserialize)]
    struct TestConversation {
        id: i64,
        title: String,
        messages: Vec<TestMessage>,
    }

    let mut group = c.benchmark_group("json_serialize");

    for &msg_count in &[10usize, 50, 200, 1000] {
        let conv = TestConversation {
            id: 1,
            title: "Test Conversation".to_string(),
            messages: (0..msg_count)
                .map(|i| TestMessage {
                    idx: i as i64,
                    role: if i % 2 == 0 { "user" } else { "assistant" }.to_string(),
                    content: format!(
                        "Message {}: Lorem ipsum dolor sit amet, consectetur adipiscing elit. \
                         Sed do eiusmod tempor incididunt ut labore et dolore magna aliqua.",
                        i
                    ),
                    created_at: Some(1700000000000 + i as i64 * 1000),
                })
                .collect(),
        };

        let estimated_size = msg_count * 200; // rough estimate
        group.throughput(Throughput::Bytes(estimated_size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}_msgs", msg_count)),
            &msg_count,
            |b, _| {
                b.iter(|| {
                    let json = serde_json::to_vec(&conv).expect("serialize");
                    black_box(json)
                })
            },
        );
    }

    group.finish();
}

/// Benchmark MessagePack serialization (used for binary metadata).
fn bench_msgpack_serialize(c: &mut Criterion) {
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize)]
    struct TestMetadata {
        agent: String,
        workspace: String,
        tags: Vec<String>,
        extra: serde_json::Value,
    }

    let metadata = TestMetadata {
        agent: "claude-code".to_string(),
        workspace: "/home/user/projects/example".to_string(),
        tags: vec![
            "rust".to_string(),
            "benchmark".to_string(),
            "test".to_string(),
        ],
        extra: serde_json::json!({
            "model": "claude-3-opus",
            "tokens": 1500,
            "duration_ms": 3200
        }),
    };

    c.bench_function("msgpack_serialize", |b| {
        b.iter(|| {
            let packed = rmp_serde::to_vec(&metadata).expect("pack");
            black_box(packed)
        })
    });

    // Pre-serialize for deserialize bench
    let packed = rmp_serde::to_vec(&metadata).expect("pack");

    c.bench_function("msgpack_deserialize", |b| {
        b.iter(|| {
            let unpacked: TestMetadata = rmp_serde::from_slice(&packed).expect("unpack");
            black_box(unpacked)
        })
    });
}

// =============================================================================
// Helpers
// =============================================================================

/// Format byte size for display.
fn format_size(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{}MB", bytes / (1024 * 1024))
    } else if bytes >= 1024 {
        format!("{}KB", bytes / 1024)
    } else {
        format!("{}B", bytes)
    }
}

// =============================================================================
// Criterion Configuration
// =============================================================================

criterion_group!(
    compress_benches,
    bench_compress_levels,
    bench_compress_scaling,
    bench_decompress,
    bench_compress_data_types
);

criterion_group!(
    chunked_benches,
    bench_chunked_compress,
    bench_streaming_compress
);

criterion_group!(roundtrip_benches, bench_roundtrip);

criterion_group!(
    serialize_benches,
    bench_json_serialize,
    bench_msgpack_serialize
);

criterion_main!(
    compress_benches,
    chunked_benches,
    roundtrip_benches,
    serialize_benches
);
