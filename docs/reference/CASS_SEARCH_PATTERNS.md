# CASS (Coding Agent Session Search) - Fast Search Patterns Analysis

## Overview
CASS uses a **multi-layered hybrid search architecture** combining lexical (BM25), semantic (embeddings), and hybrid (RRF fusion) search modes. The system prioritizes performance through intelligent indexing, caching, and lazy semantic loading.

---

## 1. CORE SEARCH ARCHITECTURE

### Primary Search Engine: Tantivy
- **Type**: Full-text search engine written in Rust
- **Role**: High-performance lexical (BM25) search backbone
- **Features**:
  - Tokenizer: Custom "hyphen_normalize" tokenizer (SimpleTokenizer + LowerCaser + RemoveLongFilter)
  - Inverted index with term frequencies and positions
  - Field-specific indexing (title vs. content)
  - Range queries for temporal filters
  - Boolean query support (AND, OR, NOT)

### Secondary: Vector Index (Semantic Search)
- **Format**: Custom binary format "CVVI" (Cass Vector Index)
- **Features**:
  - Quantization support (F32 or F16 for memory efficiency)
  - Memory-mapped file access for performance
  - Fixed-size row structure (70 bytes per entry) for efficient seeking
  - Content-addressed deduplication (SHA256 hashing)
  - Variable dimension embeddings

### Tertiary: SQLite
- **Role**: Fallback and metadata storage
- **Features**: Connection pooling, schema management

---

## 2. INDEXING STRATEGY

### Schema Definition (Tantivy)
```rust
// Text fields (tokenized, searchable)
- title        : TEXT | STORED (full-text indexed)
- content      : TEXT | STORED (full-text indexed)
- title_prefix : TEXT (edge n-gram for prefix matching, not stored)
- content_prefix : TEXT (edge n-gram for prefix matching, not stored)
- preview      : TEXT | STORED (truncated content for display)

// Exact-match fields (STRING = single token, no tokenization)
- agent        : STRING | STORED (exact agent matching)
- workspace    : STRING | STORED (exact path matching)
- source_id    : STRING | STORED (source provenance)
- origin_kind  : STRING | STORED (local vs. remote)

// Structured fields
- msg_idx      : U64 | INDEXED | STORED (message index)
- created_at   : I64 | INDEXED | STORED | FAST (timestamp filtering)
- workspace_original : STORED (pre-rewrite audit trail)
- origin_host  : STORED (remote host tracking)
```

### Edge N-gram Generation
- **Purpose**: Enable fast prefix matching without regex scanning
- **Algorithm**: Generate all n-grams from length 2 to word length
  - Example: "hello" → ["he", "hel", "hell", "hello"]
- **Storage**: Stored in `title_prefix` and `content_prefix` fields
- **Benefit**: Fast prefix queries via term matching instead of regex

### Tokenizer Configuration
```rust
TextAnalyzer {
  base: SimpleTokenizer,          // Split on whitespace/punctuation
  filters: [
    LowerCaser,                   // Normalize case
    RemoveLongFilter { limit: 40 } // Skip overly long tokens
  ]
}
```

---

## 3. QUERY EXECUTION PATTERNS

### Query Type Detection

#### IndexStrategy Enum
```rust
enum IndexStrategy {
  EdgeNgram,             // Fast path: edge n-gram prefix matching
  RegexScan,             // Regex for leading wildcards (*foo)
  BooleanCombination,    // Complex boolean expressions
  RangeScan,             // Temporal filtering (created_at)
  FullScan,              // Empty query or AllQuery
}
```

#### QueryCost Estimation
```rust
enum QueryCost {
  Low,                   // Under 10ms (typical)
  Medium,                // 10-100ms
  High,                  // 100ms+ (heavy scanning)
}
```

### Query Building Pipeline

#### 1. Boolean Query Parsing
```rust
// Input: "agent:claude AND (foo OR bar) NOT deprecated"
// Output: Structured QueryToken tree
// Supports: AND, OR, NOT operators + quoted phrases
```

#### 2. Term Query Construction
```rust
// For each term:
match WildcardPattern::parse(term_str) {
  // No wildcards -> Direct term query
  // Prefix wildcard (foo*) -> Edge n-gram match
  // Suffix wildcard (*foo) -> RegexQuery
  // Both (*foo*) -> RegexQuery
}

// Build Should clauses across fields:
- title field (higher weight via TF-IDF)
- content field
- title_prefix field
- content_prefix field
```

#### 3. Filter Application
```rust
// Applied as MUST clauses:
1. Agent filter    -> TermQuery on "agent" field
2. Workspace filter -> TermQuery on "workspace" field  
3. Time range      -> RangeQuery on "created_at"
4. Source filter   -> TermQuery on "origin_kind" ("local" vs "ssh")
5. Session paths   -> Applied post-search (source_path not indexed)
```

### Search Execution

```rust
fn search_tantivy(
  query: &str,
  filters: SearchFilters,
  limit: usize,
  offset: usize
) -> Result<Vec<SearchHit>> {
  // 1. Maybe reload reader (with debounce)
  self.maybe_reload_reader(reader)?;
  
  // 2. Parse query into tokens
  let tokens = parse_boolean_query(query);
  
  // 3. Build Tantivy query clauses
  let clauses = build_boolean_query_clauses(&tokens, fields);
  
  // 4. Add filter clauses (agent, workspace, time range, source)
  
  // 5. Execute search with snippet generation
  let top_docs = searcher.search(&q, &TopDocs::with_limit(limit).and_offset(offset))?;
  
  // 6. Convert to SearchHit structs with snippets
  // 7. Deduplicate by (source_id, content_hash)
  // 8. Apply session_paths filter (post-search)
  
  Ok(hits)
}
```

---

## 4. PERFORMANCE OPTIMIZATION LAYERS

### Layer 1: Prefix Cache (LRU In-Memory)
```rust
// Purpose: Reuse results while user types
// Mechanism:
//   - When user types "hel", cache query results
//   - When user types "hello", check if prefix "hel" was cached
//   - Filter cached results through bloom filter bloom filter gate
//   - Return if all query tokens present (Bloom gate pass + content verification)

CachedHit {
  hit: SearchHit,
  lc_content: String,        // Lowercase for fast comparison
  lc_title: Option<String>,
  lc_snippet: String,
  bloom64: u64,              // 64-bit Bloom filter for token presence
}

// Bloom filter: 1 bit per unique token (up to 64 bits)
// Fast gate before expensive string matching
```

### Layer 2: Warm Worker (Background Index Reload)
```rust
// Purpose: Preload index pages into OS cache
// Mechanism:
//   - Debounced channel: at most one reload every WARM_DEBOUNCE_MS (300ms typical)
//   - Background tokio task runs index reader reload
//   - Executes tiny test search (limit: 1 doc) to page in data
//   - Non-blocking: doesn't impact user input

// Benefits:
//   - Next user search benefits from hot OS cache
//   - Graceful handling: spawn fails silently if no Tokio runtime
```

### Layer 3: Merge Optimization
```rust
// Purpose: Reduce segment count for faster searching
// Mechanism:
//   - Segments accumulate as documents are indexed
//   - Threshold: >= 4 segments trigger merge attempt
//   - Cooldown: minimum 5 minutes between merge operations
//   - Asynchronous: runs in background (non-blocking)

pub struct MergeStatus {
  segment_count: usize,           // Current searchable segments
  last_merge_ts: i64,             // Last merge timestamp (ms)
  ms_since_last_merge: i64,       // Elapsed time since merge
  merge_threshold: usize,         // When to trigger merge
  cooldown_ms: i64,               // Minimum interval
}

pub fn optimize_if_idle(&mut self) -> Result<bool>;
```

### Layer 4: Schema Versioning
```rust
// Purpose: Detect incompatible schema changes, trigger rebuild
// Mechanism:
//   - SCHEMA_HASH = "tantivy-schema-v6-provenance-indexed"
//   - Stored in schema_hash.json at index root
//   - Mismatch -> Complete index rebuild
//   - Prevents subtle field-ID mismatches

// Current schema version: v6
// Includes provenance fields (P1.4): source_id, origin_kind, origin_host
```

### Layer 5: Snippet Generation
```rust
// Fast path (prefix-only queries):
if is_prefix_only(query) {
  // Skip SnippetGenerator, use fast prefix search
  quick_prefix_snippet(&content, &query, 160)
}

// Full-text queries:
// Use Tantivy's SnippetGenerator for context-aware snippets
// Converts to Markdown bold (**text**) for highlights
```

---

## 5. SEARCH MODES

### Mode 1: Lexical (BM25) Search
- **Algorithm**: Tantivy's BM25 scoring
- **Speed**: <10ms typical for indexed terms
- **Best for**: Keyword matching, technical terms
- **Example**: `search "rust async await"` → matches documents with these terms

### Mode 2: Semantic Search
- **Embedder**: FastEmbed (MiniLM model or hash-based fallback)
- **Quantization**: F32 or F16 for memory efficiency
- **Speed**: Depends on embedder, ~100-500ms for inference
- **Best for**: Concept matching, paraphrases
- **Fallback**: Hash-based embedder (deterministic, no model download)

### Mode 3: Hybrid Search (RRF Fusion)
- **Algorithm**: Reciprocal Rank Fusion (RRF)
  ```
  score = Σ (1 / (K + rank))
  
  where K = 60 (tunable constant)
  rank = position in ranked list (0-indexed)
  ```
- **Candidate depth**: 3x multiplier (fetch 300 candidates from each, rerank top 100)
- **Benefits**:
  - Documents in both results get boosted
  - Graceful fallback if one source has few results
  - Deterministic fusion (no randomness)

---

## 6. CACHING ARCHITECTURE

### Cache Metrics
```rust
pub struct Metrics {
  cache_hits: u64,        // Successful prefix reuse
  cache_miss: u64,        // No cache entry at all
  cache_shortfall: u64,   // Cached but insufficient (< limit)
  reloads: u64,           // Reader reloads triggered
  reload_ms_total: u64,   // Total time spent reloading
}
```

### Cache Eviction
```rust
// LRU cache with two dimensions:
//   - Capacity: max entries (default)
//   - Byte limit: max total size (default)
//
// Evicts least recently used entries when limits exceeded
```

### Cache Key
```rust
cache_key = format!(
  "v{}|schema:{}|query:{}|filters:{}",
  CACHE_KEY_VERSION,
  SCHEMA_HASH,
  sanitized_query,
  filters_fingerprint(&filters)
)

// filters_fingerprint includes:
//   - agents (sorted)
//   - workspaces (sorted)
//   - created_from/created_to
//   - source_filter
//   - session_paths (sorted)
```

---

## 7. FILTERING PATTERNS

### Pre-Search Filters (Index-aware)
```rust
// Fast: Applied via index queries before retrieving docs
1. Agent filter       -> TermQuery (STRING field, exact match)
2. Workspace filter   -> TermQuery (STRING field, exact match)
3. Time range         -> RangeQuery (I64 field with FAST flag)
4. Source origin      -> TermQuery (STRING field: "local" vs "ssh")
```

### Post-Search Filters (Content-aware)
```rust
// Applied after document retrieval:
1. Session paths      -> String contains check (source_path not indexed)
2. Deduplication      -> (source_id, normalized_content) grouping
3. Tool noise filter  -> Regex check for tool invocation markers
```

### Structured Filters
```rust
pub struct SearchFilters {
  agents: HashSet<String>,                // Agent slugs to include
  workspaces: HashSet<String>,            // Workspace paths to include
  created_from: Option<i64>,              // Start timestamp (ms)
  created_to: Option<i64>,                // End timestamp (ms)
  source_filter: SourceFilter,            // Local/Remote/SourceId
  session_paths: HashSet<String>,         // For chained searches
}

pub enum SourceFilter {
  All,                                    // No filtering
  Local,                                  // Only local sources
  Remote,                                 // Only remote (SSH) sources
  SourceId(String),                       // Specific source ID
}
```

---

## 8. DEDUPLICATION STRATEGY

### Content-based Deduplication
```rust
fn deduplicate_hits(hits: Vec<SearchHit>) -> Vec<SearchHit> {
  // Key: (source_id, normalized_content)
  // normalized_content = split_whitespace + join(" ")
  
  // Logic:
  //   Same content from SAME source -> Keep highest score
  //   Same content from DIFFERENT sources -> Keep both (P2.3 - source boundary)
  
  // Side effect: Filters tool invocation noise
  //   Pattern: [Tool: X - description]
}
```

### Source Boundary Respect (P2.3)
```rust
// Different sources = different conversations
// Same content from local and SSH sources appear separately
// Maintains clear source attribution
```

---

## 9. DEPENDENCIES FOR FAST SEARCHING

```toml
[dependencies]
# Primary search engine
tantivy = "*"                    # Full-text indexing & BM25

# Semantic search
fastembed = { features = ["ort-download-binaries"] }

# Cache management
lru = "*"                        # LRU cache for prefix reuse

# Vector operations
half = "*"                       # F16 quantization for embeddings
memmap2 = "*"                    # Memory-mapped vector access

# Data structures
parking_lot = "*"                # Fast synchronization primitives
crossbeam-channel = "*"          # Multi-producer MPMC channels

# Async/threading
tokio = { features = ["rt-multi-thread", "macros", "time"] }
rayon = "*"                      # Parallel iteration

# Persistence
rusqlite = { features = ["bundled", "modern_sqlite"] }

# Hashing
crc32fast = "*"                  # Fast CRC for checksums
sha2 = "*"                       # SHA256 for content hashing

# Utilities
itertools = "*"                  # Iterator adapters
strsim = "0.11.1"               # String similarity (fuzzy matching)
```

---

## 10. PERFORMANCE CHARACTERISTICS

### Typical Latencies
```
Prefix queries (cached):           <5ms
Term queries (indexed):            5-50ms
Phrase queries:                    20-100ms
Wildcard queries (prefix):         50-200ms
Wildcard queries (suffix regex):   100-500ms
Range queries (time filter):       10-100ms
Full-text complex (AND/OR/NOT):    50-500ms
Semantic search:                   100-1000ms (embedding inference)
Hybrid (RRF):                      100-1500ms (both engines + fusion)
```

### Memory Usage Optimization
```
F16 quantization:  50% reduction vs F32
Edge n-grams:      ~20-30% index size overhead for fast prefix matching
Memory-mapped:     OS cache management (no heap allocation)
LRU cache:         Bounded by configurable limits
```

### Scalability
```
Documents indexed:    Millions (Tantivy designed for scale)
Segment count:        Auto-merged when >= 4 (cooldown: 5min)
Query complexity:     Boolean expressions with arbitrary nesting
Concurrent searches:  Multi-threaded Tokio runtime
```

---

## 11. KEY OPTIMIZATION DECISIONS

1. **Custom CVVI Format** instead of vector DB:
   - Direct memory-mapped binary format
   - Row-oriented for cache locality
   - Content hash for deduplication
   - No external dependencies

2. **Prefix Cache** over full result set cache:
   - Smaller memory footprint
   - Bloom filter gates prevent false reuse
   - Better for typing scenarios (incremental queries)

3. **Warm Worker** for lazy index loading:
   - Doesn't block user input
   - Debounced to prevent thrashing
   - OS cache prefill reduces latency spike

4. **Source boundary** in deduplication:
   - Maintains provenance clarity
   - Different sources = different conversations
   - Prevents losing context from multiple sources

5. **Post-search filtering**:
   - Session paths not indexed (too sparse)
   - Applied after Tantivy retrieval
   - Preserves index efficiency

6. **Edge n-grams** for prefix matching:
   - Avoids regex overhead for common case
   - Leverages fast term matching
   - No scanning needed

---

## 12. QUERY EXAMPLE WALKTHROUGH

### Query: `rust async AND (tokio OR futures) NOT deprecated`

```
1. PARSING
   Input: "rust async AND (tokio OR futures) NOT deprecated"
   Tokens: [Term("rust"), Term("async"), Bool(AND), 
            Group(Term("tokio"), Bool(OR), Term("futures")), 
            Bool(NOT), Term("deprecated")]

2. CLAUSE BUILDING
   Must clauses:
   - rust     → [title_should, content_should, title_prefix_should, content_prefix_should]
   - async    → [title_should, content_should, title_prefix_should, content_prefix_should]
   - (tokio OR futures) → [BoolQuery([tokio_should, futures_should])]
   
   MustNot clause:
   - deprecated → [title_should, content_should]

3. FILTER APPLICATION
   (assuming filters provided)
   - agent: "claude" → TermQuery("claude")
   - workspace: "/home/user/project" → TermQuery("/home/user/project")
   - time range: created_from=1700000000, created_to=1700086400 → RangeQuery

4. TANTIVY EXECUTION
   searcher.search(
     BooleanQuery([
       (MUST, BoolQuery([rust_shoulds])),
       (MUST, BoolQuery([async_shoulds])),
       (MUST, BoolQuery([(OR, tokio_shoulds), (OR, futures_shoulds)])),
       (MUSTNOT, BoolQuery([deprecated_shoulds])),
       (MUST, TermQuery("claude")),
       (MUST, TermQuery("/home/user/project")),
       (MUST, RangeQuery(created_at))
     ]),
     &TopDocs::with_limit(limit).and_offset(offset)
   )

5. SCORING
   BM25 scoring on matching documents:
   - Term frequency in field
   - Inverse document frequency
   - Field weights (title > content)
   - Boost for multiple matching terms

6. SNIPPET GENERATION
   SnippetGenerator creates context snippets with highlighted matches:
   "...in Rust, the **async** keyword with **tokio** runtime..."

7. RESULT ASSEMBLY
   SearchHit {
     title: "Working with async/await in Rust",
     snippet: "...**async** keyword with **tokio** runtime...",
     content: "Full message content...",
     score: 15.73,
     agent: "claude",
     workspace: "/home/user/project",
     created_at: 1700043200000,
     match_type: Boolean,
     ...
   }
```

---

## 13. ARCHITECTURAL STRENGTHS

1. **Deterministic** - Same query always produces same results
2. **Offline-first** - No external service calls for lexical search
3. **Composable** - Lexical + semantic can be mixed via RRF
4. **Progressive** - Gracefully degrades (hash embedder fallback)
5. **Auditable** - Provenance fields track source of all results
6. **Responsive** - Multi-layer caching for fast interactivity
7. **Maintainable** - Clear separation: Tantivy (lexical), Vector (semantic), SQLite (metadata)

---

## SUMMARY TABLE

| Pattern | Technology | Speed | Memory | Use Case |
|---------|-----------|-------|--------|----------|
| Prefix matching | Edge n-grams + TermQuery | <50ms | Minimal | Typing autocomplete |
| Full-text | Tantivy BM25 | 5-100ms | Index size | Keyword search |
| Phrase | Tantivy PhraseQuery | 20-100ms | Index size | Exact sequence |
| Boolean | Tantivy BooleanQuery | 50-500ms | Query-size | Complex expressions |
| Time filter | RangeQuery | 10-100ms | Minimal | Date-based filtering |
| Semantic | FastEmbed + Vector | 100-1000ms | ~Vector size | Concept matching |
| Hybrid | RRF fusion | 100-1500ms | ~Both sizes | Best of both |
| Caching | LRU Bloom filter | <5ms | Bounded | Interactive typing |
| Dedupe | HashMap | <1ms | Minimal | Noise filtering |

