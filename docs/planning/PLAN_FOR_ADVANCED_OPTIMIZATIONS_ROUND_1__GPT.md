# PLAN_FOR_ADVANCED_OPTIMIZATIONS_ROUND_1__GPT.md

Date: 2026-01-10

Project: `coding-agent-search` (`cass`)

Goal: identify *gross* inefficiencies that materially affect latency/throughput/memory, and implement **provably isomorphic** optimizations (same outputs for same inputs, including ordering/tie-breaking), backed by explicit oracles and regression guardrails.

This document captures:
- A careful read of `AGENTS.md` + `README.md`
- Architecture understanding from code investigation
- Baseline metrics + profiling (CPU/alloc/I/O) to find real hotspots
- Opportunity matrix and proof sketches
- Changes shipped in this round (with tests as equivalence oracles)
- Next candidate optimizations (ranked, with required validation steps)

---

## 0) Hard constraints (from `AGENTS.md`)

Non-negotiables in this repo/workflow:
- **NO FILE DELETION** without explicit written permission (even files we created).
- No destructive commands (`rm -rf`, `git clean -fd`, `git reset --hard`, etc.) unless user explicitly provides the exact command and acknowledges irreversible consequences.
- Cargo only; Rust edition 2024 nightly.
- `.env` is loaded via `dotenvy`; `.env` must never be overwritten.
- No script-based repo-wide code transformations.
- After substantive changes, run:
  - `cargo check --all-targets`
  - `cargo clippy --all-targets -- -D warnings`
  - `cargo fmt --check`

---

## 1) What cass is (purpose + technical architecture)

At a high level, `cass` is a local-first search system over coding-agent logs.

### 1.1 Data flow: ingest → normalize → persist → index → search

1) **Connectors** (`src/connectors/*`)
   - Each connector knows how to detect and scan a specific agent’s data format (Codex, Claude Code, Cline, Cursor, ChatGPT, Aider, etc.).
   - Output is a common normalized structure (conversations/messages) with metadata/provenance.

2) **SQLite storage** (`src/storage/sqlite.rs`)
   - Source of truth; append-only style.
   - Stores agents, workspaces, conversations, messages, snippets, plus an FTS mirror (and provenance tables).

3) **Tantivy full-text index** (`src/search/tantivy.rs`)
   - Speed layer for lexical search.
   - Includes:
     - standard text fields (`title`, `content`)
     - edge n-gram prefix fields (`title_prefix`, `content_prefix`) for fast prefix/typeahead
     - stored `preview` for cheap snippet-like output
     - provenance fields (`source_id`, `origin_kind`, `origin_host`)

4) **Search client** (`src/search/query.rs`)
   - Query parsing (terms/phrases/boolean operators).
   - Strategy selection:
     - exact / prefix: term query, edge n-grams
     - suffix / substring: Tantivy `RegexQuery`
     - fallback: if sparse results, auto-try `*term*` patterns
   - Cache layer: sharded LRU with bloom gating for fast “typed forward” reuse.
   - Optional semantic search via a custom CVVI vector index (below).

5) **Optional semantic search**
   - **CVVI vector index** is a custom binary format with mmap-backed slabs (`src/search/vector_index.rs`).
   - Embedding sources:
     - ML (FastEmbed / MiniLM) when model files exist
     - deterministic hash embedder fallback when not.

### 1.2 Runtime entry points

- CLI/TUI entry: `src/main.rs` loads `.env`, calls `coding_agent_search::run()`.
- CLI parsing and command routing: `src/lib.rs`.
- Indexing: `src/indexer/mod.rs`:
  - detect + scan connectors (rayon parallel)
  - ingest into SQLite + Tantivy
  - commit Tantivy
  - watch mode for incremental updates

---

## 2) Methodology requirements (the “no guessing” performance workflow)

### A) Baseline first
Before proposing optimizations:
- Run a representative workload.
- Record:
  - p50/p95/p99 latency (separately for steady-state vs cold-open when relevant)
  - throughput
  - peak RSS (or another peak memory metric)
  - exact commands and environment

### B) Profile before proposing
Capture and use:
- CPU profiles (to find time hotspots)
- Allocation profiles (to find memory churn)
- I/O profiles (to identify syscall and read amplification)

### C) Equivalence oracle
For each change, define explicit oracles:
- “Golden outputs” or deterministic invariants (including ordering)
- Property-based or metamorphic tests where a full golden set is too large

Concrete oracle templates (copy/paste for new diffs):
- Lexical search result identity (robot JSON):
  - invariant: `hits.map(|h| (h.source_path, h.line_number, h.agent))` is identical, in identical order, for the same index + query + filters + limit/offset.
- “Projection oracle” for field-lazy output:
  - invariant: `search(fields=minimal)` equals `project(search(fields=full), minimal)` (order preserved).
- Semantic vector search (CVVI):
  - invariant: `results.map(|r| (r.message_id, r.chunk_idx))` identical, in identical order, for the same query embedding + filters + k.
  - (strict mode) invariant: `results.map(|r| r.score)` identical bitwise (no FP drift).
- Canonicalization for embeddings:
  - invariant: `sha256(canonicalize(text))` identical for all tested texts (unit + property tests).

### D) Isomorphism proof sketch
For each proposed diff, include:
- Why outputs cannot change, including:
  - ordering / tie-breaking
  - floating point behavior (if present)
  - RNG seeds (if present)

Important policy for this repo (default):
- If an optimization changes floating point evaluation order (SIMD reductions, parallel partial sums, fused-multiply-add), it is **not** considered isomorphic unless we explicitly gate it behind an opt-in flag and update the oracle to tolerate drift.

### E) Opportunity matrix
Rank candidates by:
```
(Impact × Confidence) / Effort
```
Focus on likely p95+/throughput wins or meaningful memory reductions.

### F) Minimal diffs
- One lever per change.
- No unrelated refactors.
- Include rollback guidance.

### G) Regression guardrails
Add thresholds/bench tests where feasible to prevent “perf backslide”.

---

## 3) Baseline workload + metrics (pre-change)

### 3.1 Representative corpus (synthetic, isolated)

To avoid indexing any real home directories, we used:
- Synthetic Codex sessions under `/tmp/cass_bench_codex/sessions` (3000 sessions × 12 msgs).
- Fully isolated cass data dir:
  - `--data-dir /tmp/cass_bench_data_isolated`
  - `HOME=/tmp/cass_bench_home` plus `XDG_*` under that home
  - `CASS_IGNORE_SOURCES_CONFIG=1` to avoid picking up remote sources config
  - `CODING_AGENT_SEARCH_NO_UPDATE_PROMPT=1` to prevent update prompt noise

### 3.2 Baseline indexing

Command:
```bash
/usr/bin/time -v env \
  HOME=/tmp/cass_bench_home \
  XDG_CONFIG_HOME=/tmp/cass_bench_home/.config \
  XDG_DATA_HOME=/tmp/cass_bench_home/.local/share \
  XDG_CACHE_HOME=/tmp/cass_bench_home/.cache \
  CODEX_HOME=/tmp/cass_bench_codex \
  CASS_IGNORE_SOURCES_CONFIG=1 \
  CODING_AGENT_SEARCH_NO_UPDATE_PROMPT=1 \
  ./target/release/cass --color=never \
  index --full --force-rebuild --data-dir /tmp/cass_bench_data_isolated --json
```

Observed:
- `conversations=3000`, `messages=36000`
- internal `elapsed_ms ≈ 1701`
- wall ≈ `1.73s`
- max RSS ≈ `287MB`

### 3.3 Baseline search (two distinct notions of “latency”)

Important nuance:
- `cass` CLI is a *one-shot process*. If you run 200 searches as 200 processes, the cost of **opening Tantivy** dominates.
- In the **TUI**, `SearchClient` stays open, so the relevant metric is **steady-state query latency**.

We measured “process-per-search” as a proxy for automation workflows and saw two regimes:
- steady-state query work (single-digit ms) exists, but can be hidden by cold-open costs (tens of ms) in one-shot CLI.

Action item for future rounds:
- split measurement into:
  - `open_ms` (open reader/index/db)
  - `query_ms` (actual query execution)

---

## 3.4) Baseline workload + metrics (post-change, with p50/p95/p99)

### 3.4.1 Indexing (11 runs; fresh data-dir each run)

Commands (one run; repeated 11× with different `--data-dir`):
```bash
/usr/bin/time -v env \
  HOME=/tmp/cass_bench_home \
  XDG_CONFIG_HOME=/tmp/cass_bench_home/.config \
  XDG_DATA_HOME=/tmp/cass_bench_home/.local/share \
  XDG_CACHE_HOME=/tmp/cass_bench_home/.cache \
  CODEX_HOME=/tmp/cass_bench_codex \
  CASS_IGNORE_SOURCES_CONFIG=1 \
  CODING_AGENT_SEARCH_NO_UPDATE_PROMPT=1 \
  ./target/release/cass --color=never \
  index --full --force-rebuild --data-dir /tmp/cass_bench_data_isolated_round1_idx_run_XX --json
```

Corpus (same as pre-change):
- `conversations=3000`, `messages=36000`

Results (N=11; nearest-rank percentiles):
- `elapsed_ms`: p50=1601, p95=1601, p99=1601 (min=1600, max=1601, mean=1600.91)
- wall (ms): p50=1635, p95=1638, p99=1638 (min=1633, max=1638, mean=1634.91)
- throughput (messages/s): p50=22485.95, p95=22500.00, p99=22500.00 (min=22485.95, max=22500.00, mean=22487.22)
- peak RSS (kB): p50=295492, p95=297260, p99=297260 (min=292552, max=297260, mean=295220.36)

### 3.4.2 Search latency (one-shot CLI process; 200 runs per query)

Command template (repeat 200× per query, discarding stdout except for parsing `_meta.elapsed_ms`):
```bash
./target/release/cass --color=never \
  search "<QUERY>" \
  --robot-meta --robot-format compact \
  --fields minimal --limit 3 \
  --data-dir /tmp/cass_bench_data_isolated_after_round1_sample
```

Results (N=200 each; nearest-rank percentiles; note these include open+query+format time inside the CLI process):
- exact (`serialize`): p50=3ms, p95=4ms, p99=4ms (min=2, max=5, mean=3.06)
- prefix (`ser*`): p50=3ms, p95=3ms, p99=4ms (min=2, max=4, mean=2.65)
- suffix (`*ialize`): p50=6ms, p95=7ms, p99=7ms (min=6, max=8, mean=6.24)
- substring (`*erial*`): p50=9ms, p95=10ms, p99=10ms (min=8, max=11, mean=9.13)
- phrase (`\"serialize benchmark\"`): p50=3ms, p95=4ms, p99=4ms (min=2, max=5, mean=2.98)

Artifacts created (left intentionally; do not delete without explicit permission):
- `/tmp/cass_bench_data_isolated_round1_idx_run_00` … `/tmp/cass_bench_data_isolated_round1_idx_run_10`
- `/tmp/cass_bench_data_isolated_round1_cpu_profile`
- `/tmp/cass_bench_data_isolated_round1_strace_profile`

---

## 3.5) Microbench baselines (Criterion) — high signal, not end-to-end

`cass` already has Criterion benches in `benches/` that isolate core subsystems (lexical search, wildcard regex, vector search, canonicalization, cache behavior).

Why these matter:
- They approximate **steady-state** TUI performance (persistent `SearchClient`) better than “one-shot CLI per query” runs.
- They pinpoint which internal hot loops are worth optimizing (or ignoring) before we touch architecture.

Run commands:
```bash
cargo bench --bench runtime_perf -- --noplot
cargo bench --bench search_perf -- --noplot
cargo bench --bench cache_micro -- --noplot
cargo bench --bench index_perf -- --noplot
```

Selected “p50/median” results (from `PLAN_FOR_ADVANCED_OPTIMIZATIONS_ROUND_1__OPUS.md`; re-run to refresh on your machine):

| Benchmark | p50/median | What it measures | Notes |
|---|---:|---|---|
| `search_latency` (40 convs) | ~10.5 µs | Tantivy lexical search (client already open) | Not comparable to CLI-per-search |
| `search_scaling/500_convs` | ~11 µs | Lexical scaling | Indicates good term-query scaling |
| `wildcard_large_dataset/substring` | ~7.5 ms | RegexQuery / DFA build + run | Matches perf hotspots in `tantivy_fst::regex` |
| `vector_index_search_10k` | ~11.2 ms | CVVI semantic scan (10k) | Linear scaling baseline |
| `vector_index_search_50k` | ~56.1 ms | CVVI semantic scan (50k) | **Major semantic hotspot** |
| `vector_index_search_50k_filtered` | ~23.5 ms | CVVI scan with filter | Filtering helps materially |
| `canonicalize_long_message` | ~951 µs | Embedding canonicalization | Index-time + query-embed overhead |
| `rrf_fusion_100_results` | ~251 µs | Hybrid merge | Low priority |
| `hash_embed_1000_docs` | ~2.68 ms | Hash embedder throughput | Mostly index-time |
| `index_small_batch` (10 convs) | ~13.3 ms | Persist+index small batch | Index-time proxy |

Key insight from the bench harness (verified in `benches/search_perf.rs`):
- The 10k/50k semantic vector search benches are built with `Quantization::F16`, so a significant fraction of time is spent converting `f16 -> f32` inside the dot product loop.

---

## 4) Profiling (pre-change)

### 4.1 CPU profiling (perf)

In many containerized environments, `perf record` may be restricted by `kernel.perf_event_paranoid` (needs CAP_PERFMON/CAP_SYS_ADMIN). In this session, recording new perf traces is blocked (`perf_event_paranoid=4`), but **existing** perf captures from earlier runs are available under `/tmp/` and can be analyzed with `sudo perf report ...`.

#### 4.1.1 Indexing CPU hotspots (from existing perf capture)

Capture artifact:
- `/tmp/cass_perf_index_root.data`

Report command:
```bash
sudo perf report --stdio --no-children -i /tmp/cass_perf_index_root.data
```

Top hotspots by self overhead (excerpt):
- 2.73% `<tantivy_stacker::expull::ExpUnrolledLinkedListWriter>::write_u32_vint`
- 2.36% `tantivy::tokenizer::simple_tokenizer::SimpleTokenStream::advance`
- 2.20% `core::str::iter::CharIndices::next`
- 2.12% `tantivy::query::bm25::compute_tf_cache`
- 1.82% `<char>::is_alphanumeric`
- 1.19% `coding_agent_search::search::tantivy::generate_edge_ngrams`
- 1.13% `sqlite3VdbeExec`

Interpretation:
- Tantivy indexing/tokenization dominates; our “title_prefix n-gram reuse” change targets a measurable slice of `generate_edge_ngrams` and the downstream allocations it triggers.

#### 4.1.2 One-shot CLI search CPU hotspots (from existing perf capture)

Capture artifact:
- `/tmp/cass_perf_search_cli_root.data`

Report command:
```bash
sudo perf report --stdio --no-children -i /tmp/cass_perf_search_cli_root.data
```

Top hotspots by self overhead (excerpt):
- 3.63% `[kernel] clear_page_erms` (page faults / cold-open memory work)
- 3.44% `tantivy::store::reader::StoreReader::read_block` (stored field reads)
- 3.69% `core::str::iter::CharIndices::next`
- 1.16% `tantivy_fst::regex::dfa::Dfa::add`
- 1.08% `tantivy_fst::regex::dfa::DfaBuilder::cached_state`
- 0.86% `tantivy::query::regex_query::RegexQuery::from_pattern`
- 1.44% `<str>::to_lowercase`

Interpretation:
- In “CLI-per-search” mode, cold-open page-fault + stored-field reads are dominant.
- Substring/suffix wildcard patterns materially pay regex/DFA build costs, supporting a bounded “compiled regex/automaton cache” proposal (with careful isomorphism proof).

### 4.2 I/O profiling (strace syscall summaries)

We used `strace -c -f` to collect syscall counts (note: strace perturbs timings heavily; the *call counts* are the useful signal).

#### 4.2.1 Indexing syscall profile

Command:
```bash
strace -c -f -o /tmp/cass_strace_index_round1.txt env \
  HOME=/tmp/cass_bench_home \
  XDG_CONFIG_HOME=/tmp/cass_bench_home/.config \
  XDG_DATA_HOME=/tmp/cass_bench_home/.local/share \
  XDG_CACHE_HOME=/tmp/cass_bench_home/.cache \
  CODEX_HOME=/tmp/cass_bench_codex \
  CASS_IGNORE_SOURCES_CONFIG=1 \
  CODING_AGENT_SEARCH_NO_UPDATE_PROMPT=1 \
  ./target/release/cass --color=never \
  index --full --force-rebuild --data-dir /tmp/cass_bench_data_isolated_round1_strace_profile --json
```

Highlights (calls):
- `futex`: 22689
- `pwrite64`: 31443
- `pread64`: 9109
- `openat`: 3330
- `fdatasync`: 194
- `renameat`: 99
- `unlink`: 5

Artifact:
- `/tmp/cass_strace_index_round1.txt`

#### 4.2.2 Search (substring wildcard) syscall profile (200 runs)

Command:
```bash
strace -c -f -o /tmp/cass_strace_search_substring_round1.txt bash -lc '
  for i in $(seq 1 200); do
    HOME=/tmp/cass_bench_home \
    XDG_CONFIG_HOME=/tmp/cass_bench_home/.config \
    XDG_DATA_HOME=/tmp/cass_bench_home/.local/share \
    XDG_CACHE_HOME=/tmp/cass_bench_home/.cache \
    CASS_IGNORE_SOURCES_CONFIG=1 \
    CODING_AGENT_SEARCH_NO_UPDATE_PROMPT=1 \
    ./target/release/cass --color=never \
      search \"*erial*\" --robot-format compact --fields minimal --limit 1 \
      --data-dir /tmp/cass_bench_data_isolated_after_round1_sample \
      >/dev/null
  done'
```

Highlights (calls across 200 runs):
- `openat`: 24221 (~121 per run)
- `mmap`: 68089 / `munmap`: 48409
- `futex`: 155597
- `pread64`: 650
- `execve`: 250

Artifact:
- `/tmp/cass_strace_search_substring_round1.txt`

Interpretation:
- One-shot CLI search performs substantial open/mmap/munmap work per invocation; for “automation workflows” this argues for (a) minimizing stored-field loads when output fields don’t require them, and/or (b) a persistent process mode (TUI/daemon) when users care about p95+ query responsiveness rather than “one-shot” invocations.

### 4.3 Allocation profiling (jemalloc + jeprof)

We used:
- profiling build: `Cargo.toml` includes `[profile.profiling]` (inherits release, `debug=true`, `strip=false`)
- runtime allocation capture via jemalloc:
  - `LD_PRELOAD=/lib/x86_64-linux-gnu/libjemalloc.so.2`
  - `MALLOC_CONF='prof:true,prof_final:true,prof_accum:true,prof_prefix:/tmp/...'`
  - analysis via `/usr/bin/jeprof --alloc_space --text --lines ...`

Indexing alloc profile (pre-change) showed:
- total allocated ≈ `1375.7 MB` for indexing 36k messages
- biggest buckets were:
  - general Rust vec growth (`finish_grow`, `try_allocate_in`)
  - SQLite allocation sites (`sqlite3MemMalloc`)
  - a notable site inside `TantivyIndex::add_messages` corresponding to repeated `title_prefix` edge-ngrams generation (discussed below)

---

## 5) Opportunity matrix (pre-change)

Scoring: higher is better.

| Candidate | Impact | Confidence | Effort | Score | Notes |
|---|---:|---:|---:|---:|---|
| Precompute conversation-constant `title_prefix` edge-ngrams once | High | High | Low | 20 | Big alloc churn reducer; trivially isomorphic |
| Short-circuit `--robot-format sessions` to skip JSON hit building | Medium | High | Low | 10 | Removes avoidable allocations/work for chaining workflows |
| Field-aware lazy hit materialization (skip stored fields/snippets when not requested) | High | High | Medium | 10 | Targets `StoreReader::read_block` + cold-open work; isomorphic for the selected output schema |
| Cache `ensure_agent/ensure_workspace` per batch | Medium | High | Medium | 6 | N+1-ish DB writes/reads; needs careful transaction semantics |
| Reuse prepared statements for hot SQLite inserts | Medium | High | Medium | 6 | Targets `sqlite3Parser`/prepare overhead during indexing; trivially isomorphic |
| Cache compiled wildcard regex/automaton (bounded LRU) in persistent clients | Medium | Medium | Medium | 4 | Targets `RegexQuery::from_pattern` + DFA build; mostly helps TUI/long-lived runs |
| Stream connector scan → ingest with bounded queue/backpressure | High | Medium | High | 3 | Reduces peak RSS; higher risk due to ordering/tie-breakers |
| Semantic search: pre-convert CVVI F16 slab → F32 at load | High | High | Medium | 6 | Removes per-query `f16 -> f32` conversion in dot product; higher RAM use |
| Semantic search: parallel exact scan w/ deterministic merge | High | Medium | Medium | 4 | Reduces semantic p95 on large indices; must preserve tie-break rules |
| Embedding canonicalization: stream + buffer reuse | Medium | High | Medium | 4 | Index-time + semantic query embed cost; must be byte-identical |

---

## 6) Changes shipped in this round (minimal + provably isomorphic)

### 6.1 Indexing: avoid repeated `title_prefix` n-gram generation

Where:
- `src/search/tantivy.rs:261` (`TantivyIndex::add_messages`)

What changed:
- precompute per-conversation values once:
  - `source_path` string
  - `workspace` string
  - `workspace_original`
  - `title` and `title_prefix = generate_edge_ngrams(title)`
  - `started_at` fallback
- reuse them for each message document in the loop

Isomorphism proof sketch:
- Previously, for each message doc, we inserted `title_prefix = f(title)` where `f` is `generate_edge_ngrams` (pure).
- Now we compute `f(title)` once and insert the same string for every message doc.
- Tantivy receives identical per-doc field values; therefore the indexed tokens and stored fields are identical; outputs cannot change.

Equivalence oracle / test:
- `src/search/tantivy.rs:785` verifies a query matching only the title-prefix matches *every* message doc in a conversation.

Allocation/throughput impact (measured):
- Indexing total allocated (jemalloc/jeprof) dropped:
  - `1375.7MB → 1261.1MB` (about 8.3% less total allocated during indexing on the 36k message corpus)
- Indexing time on the same synthetic corpus improved slightly:
  - internal `elapsed_ms` from ~`1701` to `1601`
  - wall from ~`1.73s` to `1.63s`

Rollback:
- revert the precomputation block and restore the per-message `generate_edge_ngrams(title)` call in the loop.

### 6.2 Robot sessions output: skip building unused JSON payloads

Where:
- `src/lib.rs:3672` (`output_robot_results`)

What changed:
- if `format == RobotFormat::Sessions`, we:
  - compute `BTreeSet<&str>` of `result.hits[*].source_path`
  - print one path per line
  - `return Ok(())` early
- this avoids constructing `filtered_hits` JSON (`filter_hit_fields` + truncation + budget clamp) which is unused in sessions format.

Isomorphism proof sketch:
- sessions output depends only on the set of `source_path` values present in `result.hits`.
- the optimization only removes intermediate allocations and does not change:
  - the input set of hits
  - the `BTreeSet` ordering/uniqueness semantics
- therefore stdout lines are identical for the same `result.hits`.

Equivalence oracle / test:
- `tests/cli_robot.rs:334` checks that sessions output equals the unique sorted `source_path` values from compact JSON hits (metamorphic relation across formats).

Allocation impact (measured):
- sessions search alloc profile dropped:
  - total allocated `29.4MB → 27.0MB`
- and the sessions run no longer attributes allocation to `filter_hit_fields` / `clamp_hits_to_budget`.

Rollback:
- remove the early-return and restore the previous match arm behavior (but keep the test; it will still pass).

---

## 7) Additional non-performance correctness fix required during this work

While validating connectors and tests, timestamp parsing needed a correctness adjustment:
- `src/connectors/mod.rs:240` (`parse_timestamp`) now treats typical Unix-seconds ranges as seconds and multiplies by 1000, avoiding misclassification of small millisecond-ish values.
- connector tests updated accordingly (`src/connectors/amp.rs`, `src/connectors/cline.rs`).

This is not a perf optimization, but it was necessary for correctness and test stability.

---

## 8) Next candidates (do not implement until re-baselined + re-profiled)

These are “likely” needle-movers, but must be validated with the same baseline/profile/oracle discipline.

### 8.0 Output-field laziness: avoid reading stored fields when output schema doesn’t need them

This is the highest-confidence “gross inefficiency” hinted by the existing search CPU profile:
- `StoreReader::read_block` is a top self hotspot in CLI-per-search mode.
- `strace` shows heavy `openat` + `mmap/munmap` per one-shot search invocation.

Hypothesis:
- For `--fields minimal` / `--fields summary` / `--robot-format sessions`, we can avoid:
  - loading `content` / `snippet` / `preview` stored fields
  - highlight/snippet building
  - large JSON value construction that is immediately dropped

Isomorphism plan:
- Treat “requested output fields” as the contract.
- Proof sketch: if a field is not requested, not computing it cannot affect other fields *unless* it affects ordering, scoring, or truncation budgets. Therefore:
  - ensure ranking/ordering is computed from the same underlying Tantivy scores and sort keys
  - ensure any truncation budgets are applied only to fields that are actually emitted

Oracle plan:
- Metamorphic tests:
  - `--fields minimal` results should equal “full hits” projected down to minimal (already done for sessions; generalize to arbitrary field lists).
  - For a fixed index, verify hit ordering is identical between “full” and “minimal” modes.

Implementation sketch (minimal diff):
- Thread “requested fields” into `SearchClient::search` so retrieval/materialization is conditional.
- Keep query execution + top-doc collection identical; only change the “hydrate hits” step.

Rollback:
- retain old “always hydrate full hit then filter” path behind an env var, so we can bisect regressions quickly.

### 8.1 Indexer peak RSS: stream scan → ingest with bounded backpressure

Problem:
- `src/indexer/mod.rs` currently collects `pending_batches: Vec<(&str, Vec<NormalizedConversation>)>` across all connectors before ingesting.
- This can materialize the entire corpus in memory during indexing, increasing peak RSS and risking tail latency spikes.

Hypothesis:
- streaming ingestion (per-connector or chunked) should reduce peak RSS substantially.

Isomorphism risk:
- ordering and tie-breaking could change if ingestion becomes interleaved differently.
- If any downstream logic relies on insert order (e.g., dedupe winner selection, stable sort keys), outputs could change.

Oracle plan:
- Define deterministic tie-break rules (if not already explicit) and enforce them.
- Add metamorphic tests:
  - indexing in “batch” vs “stream” mode yields identical search results for a suite of fixed queries on the same fixture corpus.

Implementation sketch (minimal diff):
- Keep connector scanning parallel, but stream conversations over a bounded channel to a single ingest worker.
- Apply backpressure so scanning cannot outpace ingest.

Rollback:
- gated behind a feature flag/env var to allow quick revert without deleting code.

### 8.2 SQLite N+1-ish overhead: cache `ensure_agent/ensure_workspace` per batch

Problem:
- `persist_conversations_batched` calls `ensure_agent` + `ensure_workspace` for each conversation, which performs SQL `INSERT ... ON CONFLICT` then `SELECT id`.
- On large corpora this can become significant overhead.

Isomorphism:
- safe if and only if:
  - resulting IDs are identical
  - transaction boundaries and uniqueness semantics remain identical

Oracle plan:
- Compare DB row counts and key sets after indexing the same corpus with and without caching.
- Keep deterministic `slug/path → id` mapping semantics.

Implementation sketch:
- Build local `HashMap<String, i64>` for agent IDs and `HashMap<PathBuf, i64>` for workspace IDs during the batch loop.

### 8.3 Wildcard search CPU: cache compiled regex/dfa for repeated patterns

Problem:
- perf showed meaningful CPU in regex DFA construction for `RegexQuery::from_pattern` during substring/suffix wildcard searches.

Isomorphism considerations:
- caching must not change which patterns are built, nor their semantics.
- must be careful with field selection and escaping rules.

Oracle plan:
- For a fixed index, ensure repeated wildcard queries produce identical hits and ordering across many runs.
- Add tests that validate escaping behavior is unchanged.

Implementation sketch:
- A small LRU mapping `(<field>, <pattern>) -> Arc<RegexQuery>` or a tantivy_fst regex object (depending on API).
- Bound size to prevent memory blowups.

### 8.4 SQLite indexing throughput: reuse prepared statements for hot paths (FTS + message inserts)

Evidence:
- CPU profiling shows non-trivial time in SQLite parsing/execution (`sqlite3Parser`, `sqlite3VdbeExec`, `fts5*` call stacks).
- The current persistence code uses many repeated `execute(...)` calls in tight loops, which can imply “prepare/parse” overhead per row unless statements are cached.

Hypothesis:
- Reusing `rusqlite::Statement` (or `prepare_cached`) for the highest-frequency INSERTs can reduce CPU and allocator pressure without changing behavior.

Isomorphism proof sketch:
- SQL text is identical; parameter bindings are identical; transaction boundaries remain identical.
- Therefore, the resulting rows and their values are identical for the same input batch.

Oracle plan:
- Compare DB counts and key sets (agents/workspaces/conversations/messages/fts tables) after indexing the same fixture corpus.
- Run search regression tests to confirm identical hit sets and ordering for representative queries.

Implementation sketch (minimal diff):
- In the batch persistence loop, prepare the hot statements once per transaction/batch and reuse them for all rows.
- Keep error handling and commit points identical.

Rollback:
- revert to per-call `execute` or gate prepared statements behind an env var (e.g., `CASS_SQLITE_PREPARED=0`).

### 8.5 CLI cold-open latency: separate `open_ms` and `query_ms`

This isn’t necessarily a code optimization, but it is essential to avoid misleading p50/p95 numbers.

Action plan:
- Adjust robot meta to report two timings:
  - `open_ms` for index/db open
  - `query_ms` for the query itself
- Keep `elapsed_ms` for backward compatibility only if explicitly desired; otherwise clarify in docs/tests.

Isomorphism:
- output JSON changes (so not isomorphic) unless gated by a new flag (e.g., `--robot-meta-v2`).

### 8.6 Semantic search hotspot: CVVI linear scan + per-element `f16 -> f32` conversion

Evidence:
- Criterion: `vector_index_search_50k` is a clear hotspot (see `benches/search_perf.rs`; built with `Quantization::F16`).
- Code path: `src/search/vector_index.rs`:
  - `VectorIndex::search_top_k` is a straight O(n) scan over rows with a top-k heap.
  - `dot_product_f16` converts every component (`f32::from(*x)`) inside the tight loop.

Why this matters:
- Semantic search cost scales linearly with row count; on larger corpora, semantic p95 will dominate “hybrid search” responsiveness unless we fix the inner loop.

Baseline characterization (from code):
- Complexity: O(n × d) mul-adds, plus heap maintenance (k is small, typically 25).
- Extra overhead for `Quantization::F16`: O(n × d) `f16 -> f32` conversions.

Back-of-envelope bandwidth sanity check (helps decide “compute-bound vs memory-bound”):
- For `n=50_000`, `d=384`, `Quantization::F16`:
  - vector bytes read per query ≈ `n × d × 2` = `38.4 MB`
  - if the bench is ~56ms, that’s ~`686 MB/s`, far below typical memory bandwidth
- Conclusion: the hot path is plausibly **compute-bound** (dot products + conversion), not DRAM bandwidth-bound.

Proposed optimization: pre-convert F16 slab to F32 once (load-time or first-use)
- For on-disk `Quantization::F16`:
  - keep file format the same (F16 on disk, compact, mmap-friendly)
  - on load, decode the slab into a `Vec<f32>` and store `VectorStorage::F32(...)` for search

Isomorphism proof sketch:
- The current score computes: `sum_i (f32::from(f16_i) * q_i)` in a fixed sequential order.
- If we precompute `x_i = f32::from(f16_i)` once and then compute `sum_i (x_i * q_i)` with the same accumulation order, each term and each addition is identical → same `score` bitwise → same ordering and outputs.

Trade-off:
- RAM: doubles vector slab memory versus keeping `f16` in memory (but disk stays compact).
- Startup: conversion cost moves to load/open; best amortized in persistent clients (TUI), or gated behind a threshold/env var.

Implementation notes (to avoid self-inflicted regressions):
- Bench uses `VectorIndex::build(..., Quantization::F16, ...)` (in-memory `Vec<f16>`). Production commonly uses `VectorStorage::Mmap` (bounds checks + pointer math). Measure both paths before assuming identical speedups.
- Before writing any explicit SIMD, check whether LLVM already auto-vectorizes the existing scalar dot product loop for your target:
  ```bash
  RUSTFLAGS=\"--emit=asm\" cargo build --release
  # inspect dot_product/dot_product_f16 for vector instructions (and confirm it doesn't reorder sums)
  ```

Oracle plan:
- Unit test: build a small `Quantization::F16` index, compute `search_top_k` results, then run the “pref32” path and assert:
  - same `(message_id, chunk_idx)` list
  - same `score` bitwise (strict isomorphism)

Rollback:
- gate behind an env var (example): `CASS_CVVI_F16_PREF32=1` and default it off until proven.

### 8.7 Semantic search throughput: parallel exact scan with deterministic merge

Goal:
- Reduce semantic p95 on larger CVVI indices while preserving exactness.

Approach (exact + deterministic):
- Partition rows into chunks and scan in parallel (rayon).
- For each chunk, compute the chunk’s exact top-k using the same score function and the same total ordering.
- Merge all chunk-top-k heaps into a final top-k and then `sort_by(score desc, message_id asc)` (same as today).

Isomorphism proof sketch:
- Each row’s score is computed identically (same arithmetic, same order).
- Any globally-top-k row must be within the top-k of its own chunk; otherwise at least k rows in that chunk outrank it, contradicting global top-k membership.
- Final sort uses the same comparator as the current implementation, so ordering and tie-breaking are identical.

Oracle plan:
- Golden test on a fixed small index where we can assert full equality of:
  - IDs, chunk_idx, and scores
  - ordering
- Property test on random small indices: sequential and parallel implementations produce identical ordered outputs for random query vectors.

Rollback:
- feature-flag or env var (example): `CASS_CVVI_PARALLEL=1` with a conservative size threshold (e.g., only parallelize when `rows.len() >= 10_000`).

### 8.8 Embedding canonicalization: streaming + buffer reuse (byte-identical)

Evidence:
- Criterion shows `canonicalize_long_message` is ~1ms-scale (see `benches/search_perf.rs`), which affects:
  - semantic indexing (building CVVI)
  - semantic query embedding (especially in interactive TUI)

Current shape (high-level):
- multiple intermediate `String` allocations: NFC normalization, markdown/code stripping, whitespace normalization, low-signal filtering, truncation.

Proposed optimization:
- keep the required NFC normalization step (needs full-string collection for combining characters)
- stream the rest in a single pass with a pre-sized output buffer and minimal intermediate allocations

Isomorphism proof sketch:
- Define canonicalization as a pure function `canon(text) -> String`.
- Implement `canon_streaming(text)` such that for all inputs, it produces exactly the same output bytes as `canon(text)` (same markdown stripping rules, same whitespace rules, same truncation).
- If `canon_streaming == canon` byte-for-byte, downstream embeddings and search outputs are unchanged.

Oracle plan:
- Unit: a corpus of representative messages with a golden `sha256(canon(text))` list.
- Property: randomized inputs (including unicode/combining marks, markdown fences) must satisfy `canon_streaming(text) == canon(text)` exactly.

Rollback:
- keep the old implementation behind a feature flag if needed (`CASS_CANON_STREAMING=0`).

### 8.9 Approximate nearest neighbor (HNSW/IVF/PQ): *not isomorphic*, opt-in only

This is a “mathy” lever that can be a massive win at large scale, but it is **not** compatible with the strict “same outputs for same inputs” requirement.

If we ever add it:
- It must be explicit opt-in (e.g., `--approximate` or `--semantic-mode approx`).
- It must preserve deterministic tie-breaking and fixed RNG seeds (where applicable) for reproducibility within the approximate regime.
- It should be scoped behind a separate index structure (don’t silently change the exact CVVI semantics).

---

## 9) Regression guardrails (current + proposed)

Current guardrails already present:
- `tests/robot_perf.rs` enforces latency thresholds for robot help/introspect/etc.

New guardrails added this round:
- sessions output metamorphic parity test (`tests/cli_robot.rs:334`)
- title_prefix matching test (`src/search/tantivy.rs:785`)

Proposed guardrails for next round:
- indexing peak RSS regression test (hard in unit tests; consider `criterion`/bench harness + CI artifact collection).
- wildcard regex query build overhead budget via a micro-benchmark that isolates `RegexQuery::from_pattern`.
- CI-level benchmark regression checks (opt-in, but high leverage):
  - run `cargo bench` for key benches (`runtime_perf`, `search_perf`) and compare against a stored baseline
  - use `critcmp` (Criterion compare tool) with a conservative threshold (e.g., fail if >10% regression)

---

## 10) Commands to validate after any next change

Always run:
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

---

## 11) Summary of what round 1 accomplished

- Found real hotspots via alloc profiling; avoided “guess optimizations”.
- Shipped two minimal, provably-isomorphic performance improvements:
  - reuse title-prefix edge-ngrams per conversation (indexing allocs/time win)
  - early-return for sessions robot format to avoid unused JSON building (alloc win)
- Added explicit equivalence tests (metamorphic + direct).

Next round focus should be:
- peak memory reduction in indexing via streaming + backpressure (but only with strong ordering/tie-break invariants)
- reducing SQLite overhead in batched persistence via ID caching
- caching regex compilation in wildcard searches (bounded, deterministic)
- semantic search wins that preserve exactness (CVVI F16 preconvert + parallel scan)
