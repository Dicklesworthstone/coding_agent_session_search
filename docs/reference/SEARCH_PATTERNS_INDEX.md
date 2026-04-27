# CASS Search Patterns - Documentation Index

This directory contains comprehensive analysis of the fast search patterns used in CASS (Coding Agent Session Search).

## Documents

### 1. **QUICK_REFERENCE.md** (Start Here!)
**Length:** 457 lines | **Format:** Markdown | **Best for:** Quick lookup

A practical quick reference guide with:
- TL;DR of what makes CASS fast (6-layer optimization)
- Search architecture stack diagram
- Performance lookup table
- Common query execution paths
- Filtering pipeline overview
- 7 quick wins for similar projects

**When to use:** You need to quickly understand a specific pattern or look up typical latencies.

---

### 2. **CASS_ARCHITECTURE_SUMMARY.txt** (Visual Overview)
**Length:** 348 lines | **Format:** Plain text with ASCII diagrams | **Best for:** Understanding the big picture

Visual ASCII representations of:
- Complete search architecture flow (query → results)
- 5 key optimization layers (prefix cache, warm worker, merging, etc.)
- Performance characteristic table
- Core technologies and dependencies
- Filtering pipeline diagram
- Memory optimization techniques
- 8 architectural strengths
- 6 unique design decisions
- File structure breakdown

**When to use:** You want to understand the overall architecture visually or present to others.

---

### 3. **CASS_SEARCH_PATTERNS.md** (Deep Dive)
**Length:** 595 lines | **Format:** Markdown | **Best for:** Technical deep dive

Comprehensive technical reference covering:

1. **Core Search Architecture** - Tantivy, vector index, SQLite roles
2. **Indexing Strategy** - Schema definition, edge n-gram generation, tokenizer
3. **Query Execution Patterns** - Index strategy enum, cost estimation, query building
4. **Performance Optimization Layers** (5 layers) - Prefix cache, warm worker, merge, schema versioning, snippet generation
5. **Search Modes** - Lexical (BM25), Semantic (embeddings), Hybrid (RRF)
6. **Caching Architecture** - Metrics, eviction, cache keys
7. **Filtering Patterns** - Pre-search, post-search, structured filters
8. **Deduplication Strategy** - Content-based grouping, source boundaries
9. **Dependencies for Fast Searching** - 20+ crates explained
10. **Performance Characteristics** - Latencies, memory usage, scalability
11. **Key Optimization Decisions** - 6 major choices explained
12. **Query Example Walkthrough** - Step-by-step example query execution
13. **Architectural Strengths** - 7 key benefits
14. **Summary Table** - Pattern comparison matrix

**When to use:** You need to understand the internals deeply or implement similar patterns elsewhere.

---

## Quick Navigation

### I want to understand...

| Topic | Resource |
|-------|----------|
| What makes CASS fast (overview) | QUICK_REFERENCE.md "TL;DR" section |
| Complete architecture diagram | CASS_ARCHITECTURE_SUMMARY.txt "SEARCH ARCHITECTURE LAYERS" |
| Performance numbers | QUICK_REFERENCE.md "Performance Lookup Table" |
| How prefix caching works | CASS_SEARCH_PATTERNS.md "Layer 1: Prefix Cache" |
| Edge n-gram generation | QUICK_REFERENCE.md "Edge N-gram Generation" |
| Filtering pipeline | CASS_ARCHITECTURE_SUMMARY.txt "FILTERING PIPELINE" |
| RRF (hybrid search) | QUICK_REFERENCE.md "RRF Formula" |
| Specific query execution | CASS_SEARCH_PATTERNS.md "Query Example Walkthrough" |
| All indexing fields | CASS_SEARCH_PATTERNS.md "Schema Definition" |
| How deduplication works | QUICK_REFERENCE.md "Deduplication Logic" |
| Warm worker optimization | CASS_ARCHITECTURE_SUMMARY.txt "LAYER 2: WARM WORKER" |
| Segment merging | QUICK_REFERENCE.md "Segment Merging" |
| Vector index format | QUICK_REFERENCE.md "Custom CVVI Format" |
| Quick wins for my project | QUICK_REFERENCE.md "Quick Wins for Similar Projects" |

---

## File Locations in CASS

Source code is organized under `/data/projects/coding_agent_session_search/src/search/`:

```
search/
├── query.rs                  (6583 lines) - Main search engine
│   • SearchClient struct
│   • search_tantivy() function
│   • Prefix cache implementation
│   • RRF fusion algorithm
│   • Deduplication logic
│   • Snippet generation
│   • Query parsing
│
├── tantivy.rs               - Index management
│   • TantivyIndex struct
│   • build_schema() - Field definitions
│   • ensure_tokenizer() - Custom analyzer
│   • Merge optimization
│   • Edge n-gram generation
│
├── vector_index.rs          - Semantic search
│   • VectorIndex struct
│   • CVVI format spec
│   • Memory-mapped access
│
├── embedder.rs              - Embedder trait
├── fastembed_embedder.rs    - ML embeddings (MiniLM)
├── hash_embedder.rs         - Hash-based fallback
├── model_manager.rs         - Model lifecycle
└── canonicalize.rs          - Text preprocessing
```

---

## Key Code Snippets

### SearchClient::search() - Main Entry Point
Located in: `/data/projects/coding_agent_session_search/src/search/query.rs` (~line 1700+)

```rust
pub fn search(
    &self,
    query: &str,
    filters: SearchFilters,
    limit: usize,
    offset: usize,
) -> Result<Vec<SearchHit>>
```

### SearchClient::search_tantivy() - Lexical Search
Located in: `/data/projects/coding_agent_session_search/src/search/query.rs` (~line 2173+)

Implements:
- Boolean query parsing
- Filter application
- Tantivy execution
- Snippet generation
- Result assembly

### TantivyIndex::build_schema() - Index Schema
Located in: `/data/projects/coding_agent_session_search/src/search/tantivy.rs`

Defines all indexed fields:
- text fields (title, content)
- edge n-gram fields (title_prefix, content_prefix)
- exact-match fields (agent, workspace)
- structured fields (created_at, msg_idx)

### Prefix Cache Mechanism
Located in: `/data/projects/coding_agent_session_search/src/search/query.rs` (~line 1700+)

```rust
CachedHit {
    hit: SearchHit,
    lc_content: String,
    lc_title: Option<String>,
    lc_snippet: String,
    bloom64: u64,  // 64-bit Bloom filter
}
```

---

## Learning Path

### For Quick Understanding (30 minutes)
1. Read QUICK_REFERENCE.md "TL;DR" section
2. Skim CASS_ARCHITECTURE_SUMMARY.txt "SEARCH ARCHITECTURE LAYERS"
3. Look up your query type in QUICK_REFERENCE.md "Common Queries"

### For Implementation (2-3 hours)
1. Read QUICK_REFERENCE.md entirely
2. Study CASS_SEARCH_PATTERNS.md sections 1-5 (architecture, indexing, queries)
3. Review CASS_SEARCH_PATTERNS.md section 11 (key decisions)

### For Deep Mastery (4-6 hours)
1. Read all three documents thoroughly
2. Study query.rs source code (6583 lines) focusing on:
   - SearchClient implementation
   - search_tantivy() function
   - Prefix cache + Bloom filter
   - RRF fusion
   - Deduplication
3. Study tantivy.rs for schema and tokenization

---

## Performance Summary

### Typical Latencies
```
Cached prefix search:     <5ms
Term query (indexed):     5-50ms
Phrase query:             20-100ms
Prefix wildcard (foo*):   50-200ms
Suffix wildcard (*foo):   100-500ms
Boolean complex:          50-500ms
Time range filter:        10-100ms
Semantic search:          100-1000ms
Hybrid (RRF):             100-1500ms
```

### Cache Hit Rates
- Interactive typing: 60-80% (incremental prefix reuse)
- Manual search: 30-50% (filter variation)

### Memory Efficiency
- F16 quantization: 50% reduction vs F32
- Edge n-grams: +20-30% index overhead
- LRU cache: Bounded by configurable limits

---

## Architecture Highlights

### 6 Optimization Layers

1. **Prefix Cache** (LRU + Bloom64) - <5ms cache hits
2. **Edge N-grams** - Fast prefix matching without regex
3. **Warm Worker** - Background index reload for OS cache
4. **Segment Merging** - Auto-optimize when 4+ segments
5. **Schema Versioning** - Automatic rebuild on mismatch
6. **Lazy Semantic** - Optional, graceful fallback

### 3 Search Engines

1. **Tantivy** (Primary) - Full-text BM25 indexing
2. **Vector Index** (Secondary) - Custom CVVI format embeddings
3. **SQLite** (Tertiary) - Metadata fallback

### 3 Search Modes

1. **Lexical** - Keyword/BM25 matching
2. **Semantic** - Embedding similarity
3. **Hybrid** - RRF fusion of both

---

## Common Questions

### Q: How fast is CASS search?
**A:** <5ms for cached prefix queries, 5-100ms for typical term queries, up to 500ms for complex patterns.

### Q: Why use edge n-grams instead of regex?
**A:** Edge n-grams use fast Tantivy term matching (5-50ms) vs regex scanning (100-500ms).

### Q: How does prefix caching work with multiple words?
**A:** Only works for prefix-matching queries (no wildcards or boolean). Bloom filter gates prevent false reuse.

### Q: What's RRF and why is it deterministic?
**A:** Reciprocal Rank Fusion combines lexical + semantic rankings. Deterministic because same input always produces same score.

### Q: Can I use these patterns in my project?
**A:** Yes! See "Quick Wins for Similar Projects" in QUICK_REFERENCE.md

---

## Dependencies

### Search Engines
- **tantivy** - Full-text indexing with BM25
- **fastembed** - ML embeddings (MiniLM)

### Caching & Performance
- **lru** - LRU cache for prefix reuse
- **parking_lot** - Fast synchronization
- **crossbeam-channel** - Multi-producer channels

### Vector Operations
- **half** - F16 quantization
- **memmap2** - Memory-mapped vectors

### Async/Runtime
- **tokio** - Async runtime for warm worker

### Persistence
- **rusqlite** - SQLite integration

---

## Testing

Key test files in `/data/projects/coding_agent_session_search/tests/`:

- **concurrent_search.rs** - Multi-threaded safety
- **search_caching.rs** - Prefix cache behavior
- **semantic_integration.rs** - Embedder + vectors
- **search_filters.rs** - Filter application
- **ranking.rs** - RRF verification
- **e2e_search_index.rs** - End-to-end workflows

---

## References

### External Documentation
- [Tantivy Docs](https://docs.rs/tantivy/)
- [FastEmbed GitHub](https://github.com/qdrant/fastembed)
- [RRF Paper](https://dl.acm.org/doi/10.1145/312624.312649)
- [BM25 Wikipedia](https://en.wikipedia.org/wiki/Okapi_BM25)

### Internal Documentation
- `/data/projects/coding_agent_session_search/README.md` - Project overview
- `/data/projects/coding_agent_session_search/AGENTS.md` - Agent integration

---

## Document Maintenance

**Last Updated:** 2026-01-07

These documents are based on code analysis of CASS at commit `1640612` ("let there be code").

### Document Generation
All three documents were programmatically generated from source code analysis:
- Schema extracted from `/src/search/tantivy.rs`
- Query logic from `/src/search/query.rs` (6583 lines)
- Vector format from `/src/search/vector_index.rs`
- Performance characteristics from code comments and test patterns

### Keeping Documentation Updated
When CASS code changes, update these sections:
- **QUICK_REFERENCE.md** "Performance Lookup Table" - if latencies change
- **CASS_SEARCH_PATTERNS.md** "Schema Definition" - if fields added/removed
- **CASS_ARCHITECTURE_SUMMARY.txt** - if architecture layers change

---

## Contact & Contribution

For questions about these patterns or to suggest improvements:
- See `/data/projects/coding_agent_session_search/README.md`
- Review `/data/projects/coding_agent_session_search/TESTING.md` for test patterns

