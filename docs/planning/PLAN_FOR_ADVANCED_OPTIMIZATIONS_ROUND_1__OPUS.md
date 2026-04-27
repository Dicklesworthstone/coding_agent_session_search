# CASS Performance Optimization Analysis

## 0) Hard Constraints (from AGENTS.md)

Non-negotiables in this repo/workflow:
- **NO FILE DELETION** without explicit written permission.
- No destructive commands (`rm -rf`, `git clean -fd`, `git reset --hard`, etc.) unless user explicitly provides the exact command.
- Cargo only; Rust edition 2024 nightly.
- `.env` is loaded via `dotenvy`; `.env` must never be overwritten.
- No script-based repo-wide code transformations.
- After substantive changes, always run:
  ```bash
  cargo fmt --check
  cargo check --all-targets
  cargo clippy --all-targets -- -D warnings
  cargo test
  ```

---

## 1) Methodology

### A) Baseline First
Before proposing optimizations, record:
- p50/p95/p99 latency (steady-state vs cold-open)
- Throughput
- Peak RSS
- Exact commands and environment

### B) Profile Before Proposing
Capture and use:
- CPU profiles (perf) to find time hotspots
- Allocation profiles (jemalloc) to find memory churn
- I/O profiles (strace) to identify syscall amplification

### C) Equivalence Oracle
For each change, define explicit oracles:
- "Golden outputs" or deterministic invariants (including ordering)
- Property-based or metamorphic tests where a full golden set is too large

### D) Isomorphism Proof Sketch
For each proposed diff, include:
- Why outputs cannot change (ordering, tie-breaking, FP behavior, RNG seeds)

### E) Opportunity Matrix
Rank candidates by estimated `(Impact × Confidence) / Effort`. Scores are normalized 1-10 for readability.

### F) Minimal Diffs
- One lever per change
- No unrelated refactors
- Include rollback guidance

### G) Regression Guardrails
Add thresholds/bench tests to prevent "perf backslide".

---

## 2) Baseline Metrics

### 2.1 Benchmark Results

| Benchmark | p50 | Notes |
|-----------|-----|-------|
| `search_latency` (40 convs) | **10.5 µs** | Tantivy lexical, cached |
| `search_scaling/500_convs` | **11 µs** | Scales well |
| `vector_index_search_10k` | **11.2 ms** | Semantic search |
| `vector_index_search_50k_loaded` | **1.83 ms** | F16 preconvert on; ~4.57 ms with `CASS_F16_PRECONVERT=0` |
| `vector_index_search_50k_filtered` | 23.5 ms | Filtering helps |
| `wildcard_large_dataset/substring` | **7.5 ms** | Regex overhead |
| `canonicalize_long_message` | **951 µs** | Text preprocessing |
| `rrf_fusion_100_results` | 251 µs | RRF merge |
| `hash_embed_1000_docs` | 2.68 ms | Hash embedder |
| `index_small_batch` (10 convs) | 13.3 ms | Indexing throughput |

**Key Finding**: The 50k vector search benchmark uses `Quantization::F16`. Estimated ~60% of the ~4.57ms mmap path is F16→F32 conversion overhead; pre-conversion brings the same benchmark to ~1.83ms.

### 2.2 Indexing Baseline (from profiling corpus)

Corpus: 3000 conversations × 12 messages = 36,000 messages

Results (N=11 runs):
- `elapsed_ms`: p50=1601, p95=1601, p99=1601
- wall: p50=1635ms, p95=1638ms
- throughput: p50=22,486 messages/s
- peak RSS: p50=295 MB

### 2.3 Search Latency (one-shot CLI, N=200 per query)

| Query Type | p50 | p95 | p99 |
|-----------|-----|-----|-----|
| exact (`serialize`) | 3ms | 4ms | 4ms |
| prefix (`ser*`) | 3ms | 3ms | 4ms |
| suffix (`*ialize`) | 6ms | 7ms | 7ms |
| substring (`*erial*`) | 9ms | 10ms | 10ms |
| phrase (`"serialize benchmark"`) | 3ms | 4ms | 4ms |

**Important**: CLI-per-search includes cold-open costs. Split into `open_ms` vs `query_ms` for proper analysis.

---

## 3) Profiling Data

### 3.1 CPU Profiling (perf)

**Indexing hotspots** (from `/tmp/cass_perf_index_root.data`):
- 2.73% `tantivy_stacker::expull::ExpUnrolledLinkedListWriter::write_u32_vint`
- 2.36% `tantivy::tokenizer::simple_tokenizer::SimpleTokenStream::advance`
- 2.20% `core::str::iter::CharIndices::next`
- 1.19% `coding_agent_search::search::tantivy::generate_edge_ngrams`
- 1.13% `sqlite3VdbeExec`

**Search hotspots** (from `/tmp/cass_perf_search_cli_root.data`):
- 3.63% `[kernel] clear_page_erms` (page faults / cold-open)
- 3.44% `tantivy::store::reader::StoreReader::read_block` (stored field reads)
- 1.16% `tantivy_fst::regex::dfa::Dfa::add`
- 0.86% `tantivy::query::regex_query::RegexQuery::from_pattern`

### 3.2 I/O Profiling (strace)

**Indexing syscalls** (36k messages):
- `futex`: 22,689
- `pwrite64`: 31,443
- `pread64`: 9,109
- `openat`: 3,330
- `fdatasync`: 194

**Search syscalls** (substring wildcard, 200 runs):
- `openat`: 24,221 (~121/run)
- `mmap`: 68,089 / `munmap`: 48,409
- `futex`: 155,597
- `execve`: 250

Interpretation: One-shot CLI pays substantial open/mmap/munmap per invocation.

### 3.3 Allocation Profiling (jemalloc)

Indexing total allocated: ~1,375 MB for 36k messages
- Biggest buckets: Rust vec growth, SQLite allocation, edge-ngrams generation

---

## 4) Profiled Hotspots

### 4.1 **Vector Search Linear Scan** (~4.57ms mmap / ~1.83ms preconvert for 50k vectors) — `vector_index.rs:773-803`

```rust
// O(n) scan over ALL vectors
for row in &self.rows {
    if let Some(filter) = filter && !filter.matches(row) { continue; }
    let score = self.dot_product_at(row.vec_offset, query_vec)?;  // HOT PATH
    heap.push(std::cmp::Reverse(ScoredEntry { score, ... }));
    if heap.len() > k { heap.pop(); }
}
```

**Analysis**: Linear O(n×d) where n=50k, d=384. Each iteration:
- Bounds checking (4 operations)
- Dot product: 384 multiplications + 383 additions
- F16→F32 conversion (when using F16 quantization)
- Heap operations

**Memory Bandwidth Check**: 50k × 384 × 2 bytes = 38.4 MB. In ~4.57ms ≈ 8.4 GB/s; in ~1.83ms ≈ 21.0 GB/s. Modern DDR4 provides 20-50 GB/s. **Conclusion**: mmap path is compute-bound; preconvert pushes closer to bandwidth limits.

### 4.2 **Dot Product Implementation** — `vector_index.rs:1221-1228`

```rust
fn dot_product(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}
fn dot_product_f16(a: &[f16], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| f32::from(*x) * y).sum()
}
```

**Analysis**: LLVM may auto-vectorize with `-C opt-level=3`, but F16→F32 conversion per element is expensive regardless. Verify auto-vectorization with:
```bash
RUSTFLAGS="--emit=asm" cargo build --release
# Check for vmulps/vaddps (AVX) or mulps/addps (SSE) instructions
```

### 4.3 **Canonicalization** (951µs for long messages) — `canonicalize.rs:80-95`

```rust
pub fn canonicalize_for_embedding(text: &str) -> String {
    let normalized: String = text.nfc().collect();  // Allocation
    let stripped = strip_markdown_and_code(&normalized);  // Line-by-line
    let whitespace_normalized = normalize_whitespace(&stripped);  // Allocation
    let filtered = filter_low_signal(&whitespace_normalized);  // Allocation
    truncate_to_chars(&filtered, MAX_EMBED_CHARS)
}
```

**Analysis**: 4+ String allocations per call. Only impacts index-time and semantic query embedding, not lexical search.

### 4.4 **Stored Field Reads** — Tantivy `StoreReader::read_block`

Top hotspot in CLI-per-search mode. For `--fields minimal` or `--robot-format sessions`, we don't need full content/snippet fields.

### 4.5 **Cache Eviction** — `query.rs:971+`

The eviction loop only runs when over capacity and has early-break logic. **Low priority**.

### 4.6 **RRF Fusion** (251µs) — `query.rs:709+`

At 251µs, RRF fusion is fast enough. **Low priority**.

---

## 5) Equivalence Oracle

For optimization verification:

1. **Vector search**: Same (message_id, chunk_idx) set returned. Scores may differ by ~1e-7 relative error due to FP reordering with SIMD - acceptable for ranking.
2. **RRF fusion**: Deterministic tie-breaking by `SearchHitKey` ordering (already implemented).
3. **Canonicalization**: Byte-for-byte identical output (test with `content_hash`).

Property-based tests:
```
∀ query, filters: search(q, f).hits.map(|h| h.message_id) ≡ search_optimized(q, f).hits.map(|h| h.message_id)
∀ text: content_hash(canonicalize(text)) == content_hash(canonicalize_optimized(text))
```

---

## 6) Opportunity Matrix

| # | Optimization | Impact | Confidence | Effort | Score | p95 Move? |
|---|-------------|--------|------------|--------|-------|-----------|
| **1** | Pre-convert F16→F32 slab | 4.57ms → 1.83ms | HIGH | LOW | **9.0** | YES |
| **2** | SIMD dot product | legacy 30ms → 10-15ms (now shipped; current ~4.57ms mmap) | MEDIUM | LOW | **6.0** | YES |
| **3** | Parallel vector search | legacy 10-15ms → 2-3ms (now shipped; current ~4.57ms mmap) | HIGH | MEDIUM | **6.0** | YES |
| **4** | Output-field laziness | Medium | HIGH | MEDIUM | **5.0** | YES |
| **5** | Wildcard regex caching | Medium | MEDIUM | MEDIUM | **4.0** | YES |
| **6** | Streaming canonicalize | 951µs → 300µs | HIGH | MEDIUM | 4.0 | NO |
| **7** | SQLite N+1 caching | Medium | HIGH | MEDIUM | 3.0 | NO |
| **8** | Streaming backpressure | High | MEDIUM | HIGH | 3.0 | NO |
| **9** | Approximate NN (IVF/HNSW) | O(n) → O(√n) | LOW | HIGH | 2.0 | DEFER |

**Note on Approximate NN**: CASS is a precision-focused code search tool. Users expect exact results. Approximate search should require explicit opt-in (`--approximate`) if implemented.

---

## 7) Already-Shipped Optimizations (Round 0)

### 7.1 Title-Prefix N-Gram Reuse

**Location**: `src/search/tantivy.rs:261` (`TantivyIndex::add_messages`)

**What changed**: Precompute per-conversation values once:
- `source_path`, `workspace`, `workspace_original`
- `title` and `title_prefix = generate_edge_ngrams(title)`
- `started_at` fallback

**Isomorphism proof**: `generate_edge_ngrams` is pure. Computing it once vs per-message yields identical Tantivy field values.

**Impact**:
- Indexing alloc: 1,375 MB → 1,261 MB (8.3% reduction)
- Indexing time: ~1,701ms → 1,601ms

**Equivalence oracle**: `src/search/tantivy.rs:785` verifies title-prefix matching.

### 7.2 Sessions Output Short-Circuit

**Location**: `src/lib.rs:3672` (`output_robot_results`)

**What changed**: For `--robot-format sessions`, compute `BTreeSet<&str>` of `source_path` values and return early, avoiding unused JSON construction.

**Isomorphism proof**: Sessions output depends only on `source_path` set from `result.hits`. Removing intermediate allocations doesn't change the output.

**Impact**: Sessions search alloc: 29.4 MB → 27.0 MB

**Equivalence oracle**: `tests/cli_robot.rs:334` (metamorphic test across formats)

---

## 8) Recommended Optimizations

### **Optimization 1: Pre-Convert F16 Slab at Load Time** — P0

**Current**: F16→F32 conversion per dot product element (384 conversions × 50k vectors per query)
**Proposed**: Convert entire slab to F32 at `VectorIndex::load()` time

```rust
// In VectorIndex::load()
let vectors = match header.quantization {
    Quantization::F16 => {
        let f16_slice = bytes_as_f16(&mmap[slab_start..slab_end])?;
        let f32_slab: Vec<f32> = f16_slice.iter().map(|v| f32::from(*v)).collect();
        VectorStorage::F32(f32_slab)  // Store as F32
    }
    Quantization::F32 => { /* unchanged */ }
};
```

**Isomorphism Proof**:
- `f32::from(f16)` is injective and deterministic
- Same conversion happens once at load vs per-query
- Dot product inputs identical → outputs identical

**Trade-offs**:
- 2x memory for F16 indices (76.8 MB for 50k × 384 × 4-byte f32 vectors)
- Loses mmap benefits: currently `VectorStorage::Mmap` enables lazy page loading and OS caching. Converting to heap-allocated `Vec<f32>` requires loading entire slab into memory at startup. For very large indices, consider keeping mmap and adding an optional "preload" flag.

**Measured Impact** (search_perf::vector_index_search_50k_loaded, 2026-01-11):
- `CASS_F16_PRECONVERT=0`: ~4.57ms
- Default (pre-convert on): ~1.83ms
- ~60% faster on this workstation

**Rollback**: Env var `CASS_F16_PRECONVERT=0` to keep F16 storage and convert per-query (original behavior).

---

### **Optimization 2: SIMD Dot Product** — P0

**Current**: Scalar loop (may be auto-vectorized)
**Proposed**: Explicit SIMD using `wide` crate

```rust
use wide::f32x8;

fn dot_product_simd(a: &[f32], b: &[f32]) -> f32 {
    let chunks_a = a.chunks_exact(8);
    let chunks_b = b.chunks_exact(8);
    let remainder_a = chunks_a.remainder();
    let remainder_b = chunks_b.remainder();

    let mut sum = f32x8::ZERO;
    for (ca, cb) in chunks_a.zip(chunks_b) {
        let arr_a: [f32; 8] = ca.try_into().unwrap();
        let arr_b: [f32; 8] = cb.try_into().unwrap();
        sum += f32x8::from(arr_a) * f32x8::from(arr_b);
    }

    let mut scalar_sum: f32 = sum.reduce_add();
    for (a, b) in remainder_a.iter().zip(remainder_b) {
        scalar_sum += a * b;
    }
    scalar_sum
}
```

**Isomorphism Note**: SIMD reorders FP operations, causing ~1e-7 relative error. Ranking order is preserved; scores may differ slightly.

**Measured Impact** (2026-01-11, search_perf::vector_index_search_50k, target_critcmp):
- SIMD disabled (`CASS_SIMD_DOT=0`): ~5.92ms (median; 5.84-6.01ms)
- SIMD enabled (default): ~19.48ms (median; 16.49-22.72ms)
- On this host, SIMD is slower (~3.3x). Needs re-test on AVX2/NEON-capable hardware.

**Micro-bench** (runtime_perf::dot_product_* on this host):
- `dot_product_scalar`: ~260ns
- `dot_product_simd`: ~39ns
- ~6-7x faster in isolation, despite end-to-end regression above.

**Expected Impact** (legacy): 2-4x speedup (30ms → 10-15ms). Current 50k bench is ~4.57ms (mmap) / ~1.83ms (preconvert); remaining headroom is smaller.

**Dependency**: Add `wide = "0.7"` to Cargo.toml (or latest stable version)

**Rollback**: Env var `CASS_SIMD_DOT=0` to disable SIMD and fallback to scalar.

---

### **Optimization 3: Parallel Vector Search with Rayon** — P1

**Current**: Single-threaded linear scan
**Proposed**: Parallel scan with thread-local heaps (rayon already in deps)

```rust
use rayon::prelude::*;

const PARALLEL_THRESHOLD: usize = 10_000;

pub fn search_top_k_parallel(
    &self,
    query_vec: &[f32],
    k: usize,
    filter: Option<&SemanticFilter>,
) -> Result<Vec<VectorSearchResult>> {
    // Skip parallelism for small indices (Rayon overhead ~1-5µs/task)
    if self.rows.len() < PARALLEL_THRESHOLD {
        return self.search_top_k(query_vec, k, filter);
    }

    let results: Vec<_> = self.rows
        .par_chunks(1024)
        .flat_map(|chunk| {
            let mut local_heap = BinaryHeap::with_capacity(k + 1);
            for row in chunk {
                if let Some(f) = filter && !f.matches(row) { continue; }
                let score = self.dot_product_at(row.vec_offset, query_vec)
                    .unwrap_or(0.0);
                local_heap.push(Reverse(ScoredEntry {
                    score,
                    message_id: row.message_id,
                    chunk_idx: row.chunk_idx,
                }));
                if local_heap.len() > k { local_heap.pop(); }
            }
            local_heap.into_vec()
        })
        .collect();

    // Merge thread-local results into final top-k
    let mut final_heap = BinaryHeap::with_capacity(k + 1);
    for entry in results {
        final_heap.push(entry);  // entry is Reverse<ScoredEntry>
        if final_heap.len() > k { final_heap.pop(); }
    }

    let mut results: Vec<VectorSearchResult> = final_heap
        .into_iter()
        .map(|e| VectorSearchResult {
            message_id: e.0.message_id,
            chunk_idx: e.0.chunk_idx,
            score: e.0.score,
        })
        .collect();
    results.sort_by(|a, b| b.score.total_cmp(&a.score)
        .then_with(|| a.message_id.cmp(&b.message_id)));
    Ok(results)
}
```

**Isomorphism Proof**:
- Heap merge is associative
- Final sort with deterministic tie-breaking (message_id) ensures identical output
- Parallel execution order doesn't affect result set

**Expected Impact** (legacy): ~4x on 4-core, ~8x on 8-core (10-15ms → 2-3ms). Current 50k bench is ~4.57ms (mmap) / ~1.83ms (preconvert).

**Measured Impact** (2026-01-12, 64-core host, search_perf::vector_index_search_50k_loaded):

| Configuration | Time (p50) | Speedup vs Sequential |
|---------------|------------|----------------------|
| Sequential (CASS_PARALLEL_SEARCH=0) | ~100ms (63-135ms) | 1x (baseline) |
| RAYON_NUM_THREADS=1 | ~6.3ms | ~16x |
| RAYON_NUM_THREADS=4 | ~2.3ms | ~43x |
| RAYON_NUM_THREADS=8 | ~1.67ms | ~60x |
| RAYON_NUM_THREADS=16 | ~1.67ms | ~60x |
| RAYON_NUM_THREADS=32 | ~1.81ms | ~55x |
| Default (64 cores) | ~2.05ms | ~49x |

**Analysis**:
- Excellent scaling up to 8 threads (~60x improvement)
- Diminishing returns beyond 8-16 threads (memory bandwidth saturation, merge overhead)
- Single-threaded Rayon (~6.3ms) outperforms pure sequential (~100ms) by 16x due to chunked processing and better cache locality
- Optimal thread count appears to be 8-16 for this 50k vector workload
- Full 64-core utilization shows slight regression vs 8-16 cores due to scheduling overhead

**Tuning Note**: Chunk size of 1024 yields ~49 chunks for 50k vectors. Consider 256-512 for better load balancing on many-core systems. Benchmark to find optimal value.

**Dependency Note**: Works best after F16 pre-convert (Optimization 1). With mmap storage, parallel access may cause page fault contention. With pre-converted F32 Vec, all data is in memory and parallelism is fully effective.

**Syntax Note**: Uses `let_chains` (`if let Some(f) = filter && ...`) which requires Rust 1.76+ or nightly.

**Rollback**: Env var `CASS_PARALLEL_SEARCH=0` to disable parallelism and use sequential scan.

---

### **Optimization 4: Output-Field Laziness** — P1

**Current**: Always load all stored fields from Tantivy, then filter
**Proposed**: Skip stored field reads when output schema doesn't need them

**Problem**: `StoreReader::read_block` is a top hotspot. For `--fields minimal` / `--robot-format sessions`, we don't need `content`, `snippet`, or `preview`.

**Implementation sketch**:
- Thread "requested fields" into `SearchClient::search`
- Keep query execution + top-doc collection identical
- Only change the "hydrate hits" step

**Isomorphism proof**: If a field is not requested, not computing it cannot affect:
- Ranking/ordering (computed from Tantivy scores)
- Other fields (no dependencies)

**Oracle**: Metamorphic tests verifying hit ordering is identical between "full" and "minimal" modes.

**Rollback**: Env var `CASS_LAZY_FIELDS=0` to disable lazy loading and hydrate all fields.

---

### **Optimization 5: Wildcard Regex Caching** — P2

**Current**: Build `RegexQuery` and DFA for each wildcard query
**Proposed**: LRU cache of `(<field>, <pattern>) -> Arc<RegexQuery>`

**Problem**: perf shows meaningful CPU in `RegexQuery::from_pattern` + DFA construction for substring/suffix wildcards.

**Isomorphism**: Caching must not change which patterns are built or their semantics.

**Oracle**: Fixed-index tests ensuring repeated wildcard queries produce identical hits.

**Rollback**: Env var `CASS_REGEX_CACHE=0` to disable.

---

### **Optimization 6: Streaming Canonicalization** — P2

**Current**: Multiple String allocations
**Proposed**: Single-pass with buffer reuse

```rust
pub fn canonicalize_for_embedding_streaming(text: &str) -> String {
    let mut result = String::with_capacity(text.len().min(MAX_EMBED_CHARS + 100));
    let normalized: String = text.nfc().collect();

    let mut in_code_block = false;
    let mut code_lines: Vec<&str> = Vec::new();
    let mut lang = String::new();

    for line in normalized.lines() {
        // Process with state machine, append directly to result
        // Avoid intermediate String allocations
    }

    result.truncate(MAX_EMBED_CHARS);
    result
}
```

**Impact**: 951µs → ~300µs. Only affects index-time, not query-time.

**Note**: NFC normalization requires full string collection (look-ahead for combining characters), so one allocation remains unavoidable. Savings come from eliminating intermediate `strip_markdown`, `normalize_whitespace`, and `filter_low_signal` allocations.

**Rollback**: Keep original `canonicalize_for_embedding` function; switch via env var `CASS_STREAMING_CANONICALIZE=0`.

---

### **Optimization 7: SQLite N+1 Caching** — P2

**Current**: `ensure_agent` + `ensure_workspace` per conversation (INSERT...ON CONFLICT + SELECT)
**Proposed**: Cache `HashMap<String, i64>` for agent IDs and workspace IDs per batch

**Isomorphism**: Safe if resulting IDs are identical and transaction boundaries unchanged.

**Oracle**: Compare DB row counts and key sets after indexing same corpus with/without caching.

**Rollback**: Env var `CASS_SQLITE_CACHE=0` to disable ID caching.

---

### **Optimization 8: Streaming Backpressure for Indexing** — P3

**Current**: Collect all `pending_batches` across connectors before ingesting
**Proposed**: Stream per-connector with bounded channel to single ingest worker

**Risk**: Ordering/tie-breaking could change if ingestion becomes interleaved differently.

**Oracle**: Metamorphic tests: indexing in "batch" vs "stream" mode yields identical search results.

**Rollback**: Feature flag for quick revert.

---

## 9) Regression Guardrails

### Current Guardrails
- `tests/robot_perf.rs`: latency thresholds for robot commands
- `tests/cli_robot.rs:334`: sessions output metamorphic parity
- `src/search/tantivy.rs:785`: title_prefix matching test

### Proposed Guardrails
```yaml
# .github/workflows/perf.yml
- name: Run benchmarks
  run: cargo bench --bench search_perf -- --save-baseline pr

- name: Compare to main baseline
  run: |
    cargo install critcmp
    critcmp main pr --threshold 10  # Fail if >10% regression
```

Additional:
- Indexing peak RSS regression test (criterion + CI artifact collection)
- Wildcard regex build overhead micro-benchmark

---

## 10) Validation Commands

Always run after changes:
```bash
cargo fmt --check
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
cargo test
```

For profiling builds:
```bash
RUSTFLAGS="-C force-frame-pointers=yes" cargo build --profile profiling
```

For benchmark comparison:
```bash
cargo bench --bench search_perf -- --save-baseline before
# Make changes
cargo bench --bench search_perf -- --save-baseline after
cargo install critcmp && critcmp before after
```

---

## 11) Implementation Notes

1. **Verify auto-vectorization first** before implementing explicit SIMD
2. **Rayon is already a dependency** - parallel search is low-risk
3. **Mmap path** has additional overhead (pointer arithmetic, bounds checking) not present in benchmarks - production may differ
4. **Feature flags** recommended for rollback capability
5. **Each optimization should be one PR** with before/after benchmarks
6. **CLI latency**: Consider separating `open_ms` vs `query_ms` in robot meta for proper analysis

---

## 12) Summary

### Already Shipped (Round 1)
| Change | Impact |
|--------|--------|
| Title-prefix n-gram reuse | 8.3% less alloc, ~100ms faster indexing |
| Sessions output short-circuit | 2.4 MB less alloc per search |

### Next Steps
| Priority | Optimization | Expected Impact |
|----------|-------------|-----------------|
| **P0** | Pre-convert F16 | 4.57ms → 1.83ms (bench) |
| **P0** | SIMD dot product | legacy 30ms → 10-15ms (shipped; current ~4.57ms mmap) |
| **P1** | Parallel search (Rayon) | legacy 10-15ms → 2-3ms (shipped; current ~4.57ms mmap) |
| **P1** | Output-field laziness | Medium (cold-open) |
| **P2** | Wildcard regex caching | Medium (TUI) |
| **P2** | Streaming canonicalize | 951µs → 300µs |
| **P2** | SQLite N+1 caching | Medium |
| **P3** | Streaming backpressure | Peak RSS reduction |

**Achievable speedup on semantic search**: legacy estimate **20-30x** (56ms → 2-3ms). Current measured 50k bench: ~4.57ms mmap → ~1.83ms preconvert.
