//! Cryptographic performance benchmarks for cass.
//!
//! Benchmarks for:
//! - Argon2id key derivation
//! - AES-256-GCM encryption/decryption
//! - HKDF key expansion
//! - Chunked encryption (large payloads)
//!
//! Run with:
//!   cargo bench --bench crypto_perf
//!
//! Performance targets:
//! | Operation | Target |
//! |-----------|--------|
//! | Argon2id derivation | < 5s (browser-compatible params) |
//! | AES-GCM encrypt 1MB | < 50ms |
//! | AES-GCM decrypt 1MB | < 50ms |
//! | Chunked encrypt 10MB | < 1s |

use coding_agent_search::encryption::{
    aes_gcm_decrypt, aes_gcm_encrypt, argon2id_hash, hkdf_expand, hkdf_extract, Argon2Params,
};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use rand::Rng;
use std::hint::black_box;

// =============================================================================
// Argon2id Benchmarks
// =============================================================================

/// Benchmark Argon2id with minimal parameters (fast development/testing).
fn bench_argon2id_minimal(c: &mut Criterion) {
    let password = b"test-password-for-benchmarking";
    let salt = [0u8; 16];

    // Minimal params: m=16KB, t=1, p=1
    let params = Argon2Params::new(16 * 1024, 1, 1, Some(32)).expect("valid params");

    c.bench_function("argon2id_minimal", |b| {
        b.iter(|| {
            let _ = black_box(argon2id_hash(password, &salt, &params));
        })
    });
}

/// Benchmark Argon2id with production-like parameters.
/// Target: < 5s on browser (where threading is limited).
fn bench_argon2id_production(c: &mut Criterion) {
    let password = b"test-password-for-benchmarking";
    let salt = [0u8; 16];

    // Production params: m=64MB, t=3, p=4
    let params = Argon2Params::new(64 * 1024, 3, 4, Some(32)).expect("valid params");

    let mut group = c.benchmark_group("argon2id_production");
    group.sample_size(10); // Fewer samples for expensive operation
    group.measurement_time(std::time::Duration::from_secs(10));

    group.bench_function("derive_key", |b| {
        b.iter(|| {
            let _ = black_box(argon2id_hash(password, &salt, &params));
        })
    });

    group.finish();
}

/// Benchmark Argon2id scaling with memory parameter.
fn bench_argon2id_memory_scaling(c: &mut Criterion) {
    let password = b"test-password-for-benchmarking";
    let salt = [0u8; 16];

    let mut group = c.benchmark_group("argon2id_memory_scaling");
    group.sample_size(10);

    // Test different memory costs: 4KB, 16KB, 64KB, 256KB
    for &mem_kb in &[4u32, 16, 64, 256] {
        let params = Argon2Params::new(mem_kb * 1024, 1, 1, Some(32)).expect("valid params");

        group.throughput(Throughput::Bytes((mem_kb * 1024) as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}KB", mem_kb)),
            &mem_kb,
            |b, _| {
                b.iter(|| {
                    let _ = black_box(argon2id_hash(password, &salt, &params));
                })
            },
        );
    }

    group.finish();
}

// =============================================================================
// AES-256-GCM Benchmarks
// =============================================================================

/// Generate random bytes for benchmarks.
fn random_bytes(len: usize) -> Vec<u8> {
    let mut rng = rand::thread_rng();
    let mut data = vec![0u8; len];
    rng.fill(&mut data[..]);
    data
}

/// Benchmark AES-GCM encryption with varying payload sizes.
fn bench_aes_gcm_encrypt(c: &mut Criterion) {
    let key = random_bytes(32);
    let nonce = random_bytes(12);
    let aad = b"cass-benchmark-aad";

    let mut group = c.benchmark_group("aes_gcm_encrypt");

    // Test payload sizes: 1KB, 64KB, 1MB, 10MB
    for &size in &[1024usize, 64 * 1024, 1024 * 1024, 10 * 1024 * 1024] {
        let plaintext = random_bytes(size);

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format_size(size)),
            &size,
            |b, _| {
                b.iter(|| {
                    let _ = black_box(aes_gcm_encrypt(&key, &nonce, &plaintext, aad));
                })
            },
        );
    }

    group.finish();
}

/// Benchmark AES-GCM decryption with varying payload sizes.
fn bench_aes_gcm_decrypt(c: &mut Criterion) {
    let key = random_bytes(32);
    let nonce = random_bytes(12);
    let aad = b"cass-benchmark-aad";

    let mut group = c.benchmark_group("aes_gcm_decrypt");

    for &size in &[1024usize, 64 * 1024, 1024 * 1024, 10 * 1024 * 1024] {
        let plaintext = random_bytes(size);
        let (ciphertext, tag) = aes_gcm_encrypt(&key, &nonce, &plaintext, aad).expect("encrypt");

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format_size(size)),
            &size,
            |b, _| {
                b.iter(|| {
                    let _ = black_box(aes_gcm_decrypt(&key, &nonce, &ciphertext, aad, &tag));
                })
            },
        );
    }

    group.finish();
}

/// Benchmark encrypt + decrypt round-trip.
fn bench_aes_gcm_roundtrip(c: &mut Criterion) {
    let key = random_bytes(32);
    let nonce = random_bytes(12);
    let aad = b"cass-benchmark-aad";

    let mut group = c.benchmark_group("aes_gcm_roundtrip");

    for &size in &[1024usize, 64 * 1024, 1024 * 1024] {
        let plaintext = random_bytes(size);

        group.throughput(Throughput::Bytes(size as u64 * 2)); // encrypt + decrypt
        group.bench_with_input(
            BenchmarkId::from_parameter(format_size(size)),
            &size,
            |b, _| {
                b.iter(|| {
                    let (ciphertext, tag) =
                        aes_gcm_encrypt(&key, &nonce, &plaintext, aad).expect("encrypt");
                    let _ = black_box(aes_gcm_decrypt(&key, &nonce, &ciphertext, aad, &tag));
                })
            },
        );
    }

    group.finish();
}

// =============================================================================
// HKDF Benchmarks
// =============================================================================

/// Benchmark HKDF extract operation.
fn bench_hkdf_extract(c: &mut Criterion) {
    let salt = random_bytes(32);
    let ikm = random_bytes(32);

    c.bench_function("hkdf_extract", |b| {
        b.iter(|| {
            let _ = black_box(hkdf_extract(&salt, &ikm));
        })
    });
}

/// Benchmark HKDF expand operation with varying output lengths.
fn bench_hkdf_expand(c: &mut Criterion) {
    let ikm = random_bytes(32);
    let salt = random_bytes(32);
    let info = b"cass-benchmark-info";

    let mut group = c.benchmark_group("hkdf_expand");

    for &len in &[32usize, 64, 128, 256] {
        group.throughput(Throughput::Bytes(len as u64));
        group.bench_with_input(BenchmarkId::from_parameter(len), &len, |b, &len| {
            b.iter(|| {
                let _ = black_box(hkdf_expand(&ikm, &salt, info, len));
            })
        });
    }

    group.finish();
}

// =============================================================================
// Chunked Encryption Benchmarks
// =============================================================================

/// Benchmark chunked encryption of large payloads.
/// This simulates the encryption pattern used for large archives.
fn bench_chunked_encrypt(c: &mut Criterion) {
    let key = random_bytes(32);
    let aad = b"cass-benchmark-aad";

    let mut group = c.benchmark_group("chunked_encrypt");
    group.sample_size(20);

    // Test with different chunk sizes
    let chunk_sizes = [64 * 1024, 256 * 1024, 1024 * 1024]; // 64KB, 256KB, 1MB
    let total_size = 10 * 1024 * 1024; // 10MB total

    for &chunk_size in &chunk_sizes {
        let plaintext = random_bytes(total_size);

        group.throughput(Throughput::Bytes(total_size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}KB_chunks", chunk_size / 1024)),
            &chunk_size,
            |b, &chunk_size| {
                b.iter(|| {
                    let mut encrypted_chunks = Vec::new();
                    for (i, chunk) in plaintext.chunks(chunk_size).enumerate() {
                        // Generate unique nonce for each chunk
                        let mut nonce = [0u8; 12];
                        nonce[0..8].copy_from_slice(&(i as u64).to_le_bytes());
                        let result = aes_gcm_encrypt(&key, &nonce, chunk, aad);
                        encrypted_chunks.push(black_box(result));
                    }
                })
            },
        );
    }

    group.finish();
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
    argon2_benches,
    bench_argon2id_minimal,
    bench_argon2id_production,
    bench_argon2id_memory_scaling
);

criterion_group!(
    aes_gcm_benches,
    bench_aes_gcm_encrypt,
    bench_aes_gcm_decrypt,
    bench_aes_gcm_roundtrip
);

criterion_group!(hkdf_benches, bench_hkdf_extract, bench_hkdf_expand);

criterion_group!(chunked_benches, bench_chunked_encrypt);

criterion_main!(
    argon2_benches,
    aes_gcm_benches,
    hkdf_benches,
    chunked_benches
);
