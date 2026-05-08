# CASS Performance Guide

This document describes performance characteristics, benchmarks, and optimization recommendations for cass (Coding Agent Session Search).

## Performance Targets

### Search Operations

| Operation | Target | Archive Size | Notes |
|-----------|--------|--------------|-------|
| Simple term search | < 100ms | 10K+ conversations | Single word queries |
| Prefix wildcard (`foo*`) | < 100ms | 10K+ conversations | Edge n-gram optimized |
| Suffix wildcard (`*bar`) | < 500ms | 10K+ conversations | Requires scan |
| Boolean queries | < 500ms | 10K+ conversations | AND/OR combinations |
| Complex queries | < 2s | 10K+ conversations | Nested boolean + wildcards |

### Answer-Pack Operations

| Operation | Target | Corpus | Notes |
|-----------|--------|--------|-------|
| Candidate hydration | Track p50/p95 | 64 / 512 / 2,048 candidates | `SearchHit` to `PackCandidate` conversion |
| Planner selection | Track p50/p95 | 64 / 512 / 2,048 candidates | Evidence scoring, freshness, dedupe, omissions |
| JSON rendering | Track p50/p95 | 512 candidates | Pretty and compact robot output serialization |
| Markdown rendering | Track p50/p95 | 512 candidates | Human handoff rendering with citations |
| Token-budget scaling | No overflow beyond planner allowance | 4K / 12K / 48K token budgets | Benchmark IDs include selected, omitted, and utilization counts |

### Cryptographic Operations

| Operation | Target | Parameters | Notes |
|-----------|--------|------------|-------|
| Argon2id derivation | < 5s | 64MB, t=3, p=4 | Browser-compatible |
| AES-GCM encrypt 1MB | < 50ms | AES-256-GCM | Authenticated encryption |
| AES-GCM decrypt 1MB | < 50ms | AES-256-GCM | Authenticated decryption |
| Chunked encrypt 10MB | < 1s | 256KB chunks | Streaming encryption |

### Database Operations

| Operation | Target | Corpus | Notes |
|-----------|--------|--------|-------|
| Database open | < 100ms | Any | Cold start |
| Insert conversation | < 10ms | Per conversation | With 10-20 messages |
| List conversations | < 50ms | 10K+ conversations | Paginated, 100 results |
| Fetch messages | < 20ms | Per conversation | Up to 500 messages |
| FTS rebuild | < 1s | 1K conversations | Full-text search index |

### Export Operations

| Operation | Target | Size | Notes |
|-----------|--------|------|-------|
| Compress 10MB | < 1s | Level 6 | DEFLATE compression |
| Decompress 10MB | < 500ms | Any level | Fast decompression |
| Full pipeline | < 2s | 10MB | Export + compress + encrypt |

## Running Benchmarks

Benchmarks are CPU-heavy. Run them through `rch` with an explicit target
directory so local agent sessions do not consume the development machine.

### Quick Benchmarks

Run all benchmarks with default settings:

```bash
rch exec -- env CARGO_TARGET_DIR=/tmp/cass-bench-target cargo bench
```

Run specific benchmark suite:

```bash
# Crypto benchmarks
rch exec -- env CARGO_TARGET_DIR=/tmp/cass-bench-crypto-target cargo bench --bench crypto_perf

# Database benchmarks
rch exec -- env CARGO_TARGET_DIR=/tmp/cass-bench-db-target cargo bench --bench db_perf

# Export/compression benchmarks
rch exec -- env CARGO_TARGET_DIR=/tmp/cass-bench-export-target cargo bench --bench export_perf

# Search benchmarks
rch exec -- env CARGO_TARGET_DIR=/tmp/cass-bench-search-target cargo bench --bench search_perf

# Answer-pack planner/render benchmarks
rch exec -- env CARGO_TARGET_DIR=/tmp/cass-bench-pack-target cargo bench --bench search_perf -- answer_pack

# Fast answer-pack SLO report with p50/p95 stage timings
rch exec -- env CARGO_TARGET_DIR=/tmp/cass-test-pack-target cargo test --test robot_perf answer_pack_planner_and_render_p95_under_slo -- --nocapture

# Indexing benchmarks
rch exec -- env CARGO_TARGET_DIR=/tmp/cass-bench-index-target cargo bench --bench index_perf

# Cache microbenchmarks
rch exec -- env CARGO_TARGET_DIR=/tmp/cass-bench-cache-target cargo bench --bench cache_micro

# Full runtime benchmarks
rch exec -- env CARGO_TARGET_DIR=/tmp/cass-bench-runtime-target cargo bench --bench runtime_perf
```

### Filtered Benchmarks

Run specific benchmark functions:

```bash
# Only Argon2 benchmarks
rch exec -- env CARGO_TARGET_DIR=/tmp/cass-bench-filter-target cargo bench -- argon2

# Only compression benchmarks
rch exec -- env CARGO_TARGET_DIR=/tmp/cass-bench-filter-target cargo bench -- compress

# Only scaling benchmarks
rch exec -- env CARGO_TARGET_DIR=/tmp/cass-bench-filter-target cargo bench -- scaling
```

### CI/Release Benchmarks

For thorough benchmarking with more samples:

```bash
# Increase sample size for more accurate results
rch exec -- env CARGO_TARGET_DIR=/tmp/cass-bench-release-target cargo bench -- --sample-size 100

# Save baseline for regression detection
rch exec -- env CARGO_TARGET_DIR=/tmp/cass-bench-baseline-target cargo bench -- --save-baseline main

# Compare against baseline
rch exec -- env CARGO_TARGET_DIR=/tmp/cass-bench-baseline-target cargo bench -- --baseline main
```

## Benchmark Suites

### crypto_perf.rs

Cryptographic operation benchmarks:

- **argon2id_minimal**: Fast Argon2id with minimal parameters (dev/testing)
- **argon2id_production**: Production-grade Argon2id parameters
- **argon2id_memory_scaling**: Memory cost vs. performance tradeoffs
- **aes_gcm_encrypt**: AES-256-GCM encryption at various payload sizes
- **aes_gcm_decrypt**: AES-256-GCM decryption at various payload sizes
- **aes_gcm_roundtrip**: Full encrypt + decrypt cycle
- **hkdf_extract**: HKDF key extraction
- **hkdf_expand**: HKDF key expansion
- **chunked_encrypt**: Large payload chunked encryption

### db_perf.rs

Database operation benchmarks:

- **db_open**: SQLite database open time
- **db_open_with_data**: Open time with existing data
- **db_open_readonly**: Read-only mode open time
- **insert_conversation**: Single conversation insertion
- **insert_batch**: Batch conversation insertion
- **list_conversations**: Paginated conversation listing
- **fetch_messages**: Message retrieval per conversation
- **list_agents**: Agent listing performance
- **list_workspaces**: Workspace listing performance
- **fts_rebuild**: FTS5 index rebuild time
- **daily_histogram**: Daily statistics query
- **session_count_range**: Session counting in date range
- **db_scaling**: Performance scaling with corpus size

### export_perf.rs

Export and compression benchmarks:

- **compress_levels**: DEFLATE at levels 1, 6, 9
- **compress_scaling**: Compression with varying data sizes
- **decompress**: Decompression performance
- **compress_data_types**: Compressible vs. random vs. mixed data
- **chunked_compress**: Large file chunked compression
- **streaming_compress**: Incremental streaming compression
- **roundtrip**: Full compress + decompress cycle
- **json_serialize**: JSON serialization of conversation data
- **msgpack_serialize**: MessagePack binary serialization

### search_perf.rs

Search operation benchmarks:

- **hash_embed_1000_docs**: Hash-based document embedding
- **hash_embed_batch**: Batch embedding performance
- **canonicalize_long_message**: Text canonicalization
- **canonicalize_with_code**: Code block canonicalization
- **vector_search_scaling**: Vector search at various corpus sizes
- **rrf_fusion**: Rank reciprocal fusion performance
- **answer_pack_candidate_hydration**: Search hit to answer-pack candidate conversion at 64 / 512 / 2,048 candidates
- **answer_pack_planner_scaling**: Planner selection, freshness, dedupe, omitted-item accounting, and token-budget utilization
- **answer_pack_renderers**: JSON pretty, JSON compact, and Markdown rendering cost for selected evidence
- **answer_pack_token_budget_scaling**: Planner plus JSON rendering at 4K / 12K / 48K soft token budgets

Answer-pack benchmark IDs include candidate count, selected evidence count,
omitted candidate count, and token-budget utilization percentage. The hydration
benchmarks include a heap-footprint proxy in KiB based on candidate struct and
owned string sizes. Treat a same-host p95 or Criterion typical-estimate drift
over 10% as an investigation trigger and over 20% as a regression unless the
artifact explains a deliberate tradeoff.

#### Answer-Pack Token-Budget Tuning

Answer packs are intended for copy-paste handoff contexts, so tune them by the
receiving model's available context rather than by raw archive size. A practical
default is:

```bash
cass pack "handoff query" --robot --max-tokens 12000 --limit 40 --max-evidence 8
```

Use the SLO report before changing defaults. It prints p50/p95 stage timings,
candidate count, selected evidence count, omitted count, token utilization, and
a memory proxy, which are the fields needed to tell whether a pack is too slow,
too sparse, or simply under budget.

Guidelines:

1. Prefer `--max-evidence` and `--max-sessions` for compact handoffs that still
   preserve high-signal citations.
2. Lower `--max-excerpt-chars` only after evidence/session caps are reasonable;
   very short excerpts can make citations hard to evaluate.
3. Use `--sessions-from -` when a prior `cass search --robot-format sessions`
   already identified the relevant files; this limits planner work and keeps
   evidence scoped to the intended handoff.
4. Treat very low utilization with many omitted candidates as a prompt to raise
   `--max-tokens`; treat high utilization with few selected items as a prompt to
   reduce excerpt length or evidence count.

### runtime_perf.rs

Full runtime benchmarks:

- **cold_start**: Application cold start time
- **warm_search**: Search with warm cache
- **concurrent_search**: Parallel search performance
- **memory_pressure**: Performance under memory pressure

## Optimization Recommendations

### Search Performance

1. **Use prefix wildcards over suffix**: `foo*` is faster than `*foo`
2. **Limit result count**: Use `--limit` to cap expensive queries
3. **Use field masks**: `--fields minimal` reduces data transfer
4. **Warm the cache**: First search may be slower; cache improves subsequent queries

### Memory Management

1. **Tune cache sizes**: Set `CASS_CACHE_TOTAL_CAP` based on available memory
2. **Use byte limits**: Set `CASS_CACHE_BYTE_CAP` to prevent unbounded growth
3. **Monitor memory**: Use `cass health --json` to check memory usage

### Database Performance

1. **Use readonly mode**: Open databases read-only when not writing
2. **Batch operations**: Group insertions for better throughput
3. **Maintain indexes**: Run periodic FTS rebuilds if needed

### Cryptographic Performance

1. **Tune Argon2 parameters**: Balance security vs. derivation time
2. **Choose chunk size**: Larger chunks reduce overhead, smaller improve streaming
3. **Use hardware acceleration**: Ensure AES-NI is available on the platform

## Environment Variables

| Variable | Default | Purpose |
|----------|---------|---------|
| `CASS_CACHE_SHARD_CAP` | 256 | Max entries per cache shard |
| `CASS_CACHE_TOTAL_CAP` | 2048 | Total cache entry limit |
| `CASS_CACHE_BYTE_CAP` | 0 (disabled) | Total cache byte limit |
| `CASS_PARALLEL_SEARCH` | 10000 | Threshold for parallel vector search |
| `CASS_WARM_DEBOUNCE_MS` | 120 | Debounce for warm worker |

## Profiling

### CPU Profiling

```bash
# Using perf (Linux)
rch exec -- env CARGO_TARGET_DIR=/tmp/cass-profile-search-target perf record --call-graph dwarf cargo bench --bench search_perf
# Retrieve the generated perf.data from the rch worker before opening it locally.
perf report

# Using Instruments (macOS)
# Use Instruments only in an explicitly approved local profiling session.
```

### Memory Profiling

```bash
# Using heaptrack (Linux)
rch exec -- env CARGO_TARGET_DIR=/tmp/cass-profile-db-target heaptrack cargo bench --bench db_perf
# Retrieve the generated heaptrack profile from the rch worker before opening it locally.
heaptrack_gui heaptrack.*.gz

# Using DHAT (via valgrind)
rch exec -- env CARGO_TARGET_DIR=/tmp/cass-profile-cache-target valgrind --tool=dhat cargo bench --bench cache_micro
```

### Flamegraphs

```bash
# Ensure cargo-flamegraph is already installed on the rch worker.

# Generate flamegraph
rch exec -- env CARGO_TARGET_DIR=/tmp/cass-flamegraph-target cargo flamegraph --bench search_perf -- --bench
```

## Baseline Results

Results from CI on standard hardware (8 cores, 32GB RAM):

```
argon2id_minimal        [147.2 µs]
argon2id_production     [1.23 s]
aes_gcm_encrypt/1MB     [3.2 ms]
aes_gcm_decrypt/1MB     [2.9 ms]
compress_scaling/1MB    [24.3 ms]
decompress/1MB          [8.1 ms]
db_open                 [12.4 ms]
list_conversations/100  [3.2 ms]
hash_embed_1000_docs    [45.2 ms]
```

Note: Actual results vary based on hardware. Use `--save-baseline` to track your specific environment.

## Version History

| Version | Changes |
|---------|---------|
| 0.1.57 | Initial performance benchmarks and documentation |
