# CASS Quick Reference (Agent Quickstart + Performance)

## Agent Quickstart (Robot Mode)

⚠️ NEVER run bare `cass` in an agent context — it launches the TUI. Always use `--robot` or `--json`.

```bash
# Health + index
cass health --json || cass index --full

# Search (minimal payload for LLMs)
cass search "auth error" --robot --limit 5 --fields minimal

# Build a cited handoff pack after search narrows the question
cass pack "auth error root cause" --robot --max-tokens 12000 --limit 40

# Inspect a hit (use source_path + line_number from search output)
cass view /path/to/session.jsonl -n 42 --json
cass expand /path/to/session.jsonl -n 42 -C 3 --json

# Machine-readable help
cass robot-docs guide
cass robot-docs schemas
```

**Key flags**
- `--robot` / `--json`: machine-readable output (stdout only)
- `--fields minimal`: lowest-token payload
- `--limit N`: cap results
- `--agent NAME`: filter (claude, codex, cursor, gemini, aider, etc.)
- `--days N`: recent window

**Answer-pack handoffs**
- Use `cass pack "query" --robot` when another agent or human needs a compact,
  cited evidence bundle. It is extractive and does not replace `export-html` for
  full session review.
- Check `health`, `freshness`, `privacy`, and `warnings` before copying a pack
  into another prompt.
- Tight budget:
  `cass pack "auth error root cause" --robot --max-tokens 4000 --max-evidence 8 --max-sessions 3`
- Strict freshness:
  `cass pack "auth error root cause" --robot --freshness-policy strict --freshness-window-seconds 604800 --require-evidence`
- Search-to-pack:
  `cass search "auth error" --robot-format sessions | cass pack "auth error root cause" --robot --sessions-from -`

---

## TL;DR: What Makes CASS Fast

CASS achieves **sub-10ms** interactive search through a 6-layer optimization strategy:

1. **Prefix Cache** (LRU + 64-bit Bloom filter) → <5ms cache hits
2. **Edge N-grams** (pre-computed term prefixes) → Fast prefix matching
3. **Warm Worker** (background index reload) → Pre-paged OS cache
4. **Segment Merging** (automatic on 4+ segments) → Fewer segments to search
5. **Schema Versioning** → Automatic rebuild on schema mismatch
6. **Lazy Semantic** → Optional MiniLM; hybrid queries fail open to lexical

---

## Search Architecture Stack

```
User Query
    ↓
Parse & Optimize
    ↓
Check Prefix Cache ← [HIT: <5ms] or Continue ↓
    ↓
Tantivy Full-Text Search [5-100ms typical]
    ├─ BooleanQuery parsing (AND/OR/NOT)
    ├─ Term queries (exact match)
    ├─ RangeQuery (time filters)
    ├─ RegexQuery (suffix/both-side wildcards)
    └─ BM25 scoring
    ↓
Optional: Semantic Search [100-1000ms]
    ├─ Native MiniLM embeddings when installed
    └─ Explicit hash mode when requested (degraded, non-semantic)
    ↓
Optional: RRF Hybrid Fusion [+100-500ms]
    └─ Reciprocal Rank Fusion (K=60)
    ↓
Post-Search Processing
    ├─ Session paths filter
    ├─ Deduplication (source_id, content)
    └─ Noise filtering
    ↓
Cache for Next Query
    └─ CachedHit (Bloom64 gate)
    ↓
Return Ranked Results
```

---

## Key Indexing Decisions

| Field | Type | Purpose | Query Type |
|-------|------|---------|-----------|
| title | TEXT (tokenized) | Full-text search | BM25 scoring |
| content | TEXT (tokenized) | Full-text search | BM25 scoring |
| title_prefix | EDGE N-GRAM | Prefix matching | Fast term query |
| content_prefix | EDGE N-GRAM | Prefix matching | Fast term query |
| agent | STRING (single token) | Exact matching | TermQuery (no tokenization) |
| workspace | STRING (single token) | Exact matching | TermQuery (no tokenization) |
| created_at | I64 (FAST flag) | Range filtering | RangeQuery |
| source_id | STRING | Provenance tracking | TermQuery |

---

## Performance Lookup Table

| Pattern | Speed | Implementation |
|---------|-------|-----------------|
| Cached prefix | <5ms | LRU + Bloom64 gate |
| Term query (indexed) | 5-50ms | Direct inverted index |
| Phrase query | 20-100ms | Position index |
| Prefix wildcard (foo*) | 50-200ms | Edge n-gram term |
| Suffix wildcard (*foo) | 100-500ms | RegexQuery scan |
| Boolean complex | 50-500ms | BooleanQuery nesting |
| Time range filter | 10-100ms | RangeQuery |
| Semantic search | 100-1000ms | Native MiniLM inference (frankensearch) |
| Hybrid (RRF) | 100-1500ms | Dual execution |

---

## Caching Strategy

### Prefix Cache Mechanism
```
User types: "h"     → Search + Cache result (hits: [doc1, doc2, ...])
User types: "he"    → Filter cached hits via Bloom gate
User types: "hel"   → Refine from cache, still valid
User types: "hello" → Still matches cached results (all tokens present)
User deletes: "hell"→ Re-search (new cache entry)
```

### Cache Key Components
```
version | schema_hash | query | agents | workspaces | time_range | source_filter | session_paths
```

### Hit Rate
- **Interactive typing**: 60-80% (incremental queries reuse prefix results)
- **Manual search**: 30-50% (depends on filter variation)

---

## Filtering Pipeline

### Pre-Search (Index-aware)
Applied as MUST clauses in BooleanQuery:
- **Agents**: TermQuery on `agent` field (STRING/exact)
- **Workspaces**: TermQuery on `workspace` field (STRING/exact)
- **Time Range**: RangeQuery on `created_at` field (I64/FAST)
- **Source**: TermQuery on `origin_kind` (local/ssh)

### Post-Search (Content-aware)
Applied after document retrieval:
- **Session Paths**: String contains check (source_path not indexed)
- **Deduplication**: Group by (source_id, normalized_content), keep max score
- **Noise Filtering**: Regex check for tool invocations `[Tool: ...]`

---

## RRF (Reciprocal Rank Fusion) Formula

Used in Hybrid search mode to combine lexical + semantic results:

```
score = Σ (1 / (K + rank))

where:
  K = 60 (constant, tunable)
  rank = position in result list (0-indexed)
```

**Example:**
```
Lexical results: [DocA@0, DocB@1, DocC@2]
Semantic results: [DocA@0, DocD@1, DocB@2]

RRF scores:
  DocA: 1/(60+0) + 1/(60+0) = 0.0333 (highest - appears in both)
  DocB: 1/(60+1) + 1/(60+2) = 0.0309
  DocC: 1/(60+2)           = 0.0161
  DocD: 1/(60+1)           = 0.0163

Final ranking: DocA > DocB > DocD > DocC
```

---

## Bloom Filter Gate (64-bit)

Fast pre-check to prevent false reuse of cached results:

```rust
// Hash each token to 1 of 64 bits
for token in query.tokens {
  bit_position = hash(token) % 64;
  bloom64 |= (1 << bit_position);
}

// Check: all query tokens must have bits set in cached hit
bool valid = (cached_hit.bloom64 & query.bloom64) == query.bloom64
```

**Benefits:**
- Fast gate before expensive string matching
- False positives possible (worst case: re-search)
- False negatives impossible (always catches misses)

---

## Edge N-gram Generation

Pre-computed prefix matching without regex:

```
Word: "async"

N-grams:
  Length 2: "as"
  Length 3: "asy"
  Length 4: "asyn"
  Length 5: "async"

Storage: All 4 n-grams in `title_prefix` or `content_prefix` field

Query: "asy*"
  → TermQuery for "asy" in title_prefix field
  → 50-100ms (fast term match, no regex scan)
```

---

## Schema Versioning

Automatic detection + rebuild:

```
SCHEMA_HASH = "tantivy-schema-v6-provenance-indexed"

On startup:
  1. Read schema_hash.json from index
  2. Compare to current SCHEMA_HASH
  3. Mismatch? Delete & rebuild entire index
  
This prevents subtle field-ID mismatches.
```

---

## Frankensearch FSVI Format (Vector Index)

The frankensearch vector index stores semantic vectors in `.fsvi` artifacts:

```
Header contract:
  Embedder ID + exact vector-space revision
  Dimension + quantization (F32/F16)
  Record count and format integrity metadata

Records:
  Stable cass document ID (message/chunk/provenance metadata)
  Quantized vector payload
```

**Advantages:**
- No external vector service
- Memory-mapped for efficient access
- Rejects mixed embedder revisions or dimensions
- F16 quantization saves 50% memory

---

## Warm Worker (Background Optimization)

Proactive index page loading:

```
User stops typing for 300ms
  ↓
Warm worker triggers (debounced MPMC channel)
  ↓
Dedicated `cass-warm-worker` thread:
  1. Call reader.reload() (no-op if fresh)
  2. Run mini search (limit: 1 doc) to page in OS cache
  3. Record reload metrics
  ↓
Next user search benefits from hot OS cache
```

**Non-blocking:** Doesn't delay user input

---

## Segment Merging (Auto-Optimization)

Tantivy index fragmentation management:

```
Segments accumulate during indexing:
  Segment 1 (100 docs)
  Segment 2 (200 docs)
  Segment 3 (150 docs)
  Segment 4 (180 docs)  ← Threshold hit (4 segments)

Merge triggered:
  - Only if >= 4 segments AND 5 minutes since last merge
  - Runs asynchronously in background
  - Reduces per-query cost (fewer segments to search)
  - No user-facing latency
```

---

## Dependencies (Core)

```toml
tantivy = "*"              # Full-text search engine (BM25)
frankensearch = { features = ["lexical", "ann", "native"] } # BM25 + native MiniLM + vectors
lru = "*"                  # LRU cache for prefix reuse
half = "*"                 # F16 quantization
memmap2 = "*"              # Memory-mapped vectors
asupersync = "*"           # Async runtime (warm worker)
frankensqlite = "*"        # SQLite source of truth
```

---

## Common Queries & Their Execution Paths

### Simple Term: `"rust"`
```
Parse: token("rust")
  ↓
Check prefix cache: "r", "ru", "rus", "rust"
  ↓
Build clauses: Must(BoolQuery([
    Should(TermQuery(title, "rust")),
    Should(TermQuery(content, "rust")),
    Should(TermQuery(title_prefix, "rust")),
    Should(TermQuery(content_prefix, "rust"))
  ]))
  ↓
Tantivy BM25 scoring
  ↓
Speed: 5-50ms (cached) or 20-100ms (uncached)
```

### Phrase: `"async await"`
```
Parse: phrase("async await")
  ↓
Skip prefix cache (phrase not prefix-friendly)
  ↓
Build: PhraseQuery with position index
  ↓
Tantivy phrase matching
  ↓
Speed: 20-100ms
```

### Wildcard: `"rust*"`
```
Parse: wildcard(prefix, "rust")
  ↓
Build: TermQuery on title_prefix/content_prefix with "rust" → "rust" (full word)
  ↓
Speed: 50-200ms (edge n-gram term, not regex)
```

### Wildcard: `"*async"`
```
Parse: wildcard(suffix, "async")
  ↓
Can't use edge n-grams (suffix doesn't align)
  ↓
Build: RegexQuery for /.*async/
  ↓
Tantivy regex scan
  ↓
Speed: 100-500ms (more expensive)
```

### Boolean: `"rust AND (tokio OR futures)"`
```
Parse: term("rust") AND (term("tokio") OR term("futures"))
  ↓
Build: Must([
    Must(BoolQuery([rust_shoulds])),
    Must(BoolQuery([(Should, tokio_shoulds), (Should, futures_shoulds)]))
  ])
  ↓
Tantivy boolean execution
  ↓
Speed: 50-500ms (complex nesting)
```

### With Filters: `"rust" agent:claude workspace:"/home/user"`
```
Parse query + filters
  ↓
Build clauses:
  Must([
    query_clauses,
    TermQuery(agent="claude"),
    TermQuery(workspace="/home/user")
  ])
  ↓
Tantivy executes combined query
  ↓
Speed: 20-100ms (filters pre-narrow before text search)
```

---

## Deduplication Logic

Groups identical content within a source, keeps highest-scored:

```rust
Key: (source_id, normalized_content)

Example:
  Hit 1: source="local",  content="foo bar", score=5.0
  Hit 2: source="local",  content="foo bar", score=3.0  ← Dropped
  Hit 3: source="ssh",    content="foo bar", score=4.0  ← Kept (different source)

Result: 2 hits (P2.3: respect source boundaries)
```

---

## Testing Strategy

Key test patterns found in `tests/`:

- `concurrent_search.rs` - Multi-threaded query safety
- `semantic_integration.rs` - Embedder + vector index
- `search_filters.rs` - Filter application
- `ranking.rs` - RRF fusion verification
- `search_caching.rs` - Prefix cache behavior
- `e2e_search_index.rs` - End-to-end workflows

---

## Quick Wins for Similar Projects

1. **Use edge n-grams** instead of regex for prefix matching
2. **Cache partial results** (prefix + Bloom filter gate) instead of full sets
3. **Separate pre/post filters** - index what you can, filter after if needed
4. **Schema versioning** - catch breaking changes automatically
5. **Background warm worker** - don't block on first user search
6. **RRF fusion** - deterministic way to combine dual rankings
7. **Content-addressed dedup** - normalize before grouping

---

## File Locations

```
/data/projects/coding_agent_session_search/
├── CASS_SEARCH_PATTERNS.md           ← Detailed technical reference (this file)
├── CASS_ARCHITECTURE_SUMMARY.txt     ← Visual architecture overview
├── QUICK_REFERENCE.md                ← This quick reference card
└── src/search/
    ├── query.rs                      ← 6583 lines: SearchClient, caching, RRF
    ├── tantivy.rs                    ← Index mgmt, schema, merging
    ├── vector_index.rs               ← frankensearch FSVI facade, semantic filtering
    ├── embedder.rs                   ← Embedder trait
    ├── fastembed_embedder.rs         ← Native MiniLM embeddings
    └── hash_embedder.rs              ← Explicit hash-vector tier
```

---

## Further Reading

- **Tantivy docs**: https://docs.rs/tantivy/
- **frankensearch source**: https://github.com/Dicklesworthstone/frankensearch
- **RRF paper**: https://dl.acm.org/doi/10.1145/312624.312649
- **BM25 algorithm**: https://en.wikipedia.org/wiki/Okapi_BM25
