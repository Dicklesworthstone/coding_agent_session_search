# CASS Performance Limits and Constraints

This document describes the tested performance limits and resource constraints for cass (Coding Agent Session Search).

## Archive Size Limits

| Archive Size | Conversations | Messages | Expected Performance |
|--------------|---------------|----------|---------------------|
| 10MB | 1,000 | 10,000 | Full performance, <1s search |
| 100MB | 10,000 | 100,000 | Search under 5s |
| 500MB | 50,000 | 500,000 | Search under 10s |
| 1GB+ | 100,000+ | 1,000,000+ | May require increased timeouts |

### Recommendations

- For archives under 10,000 conversations, expect near-instant search results
- For larger archives, use `--limit` to cap result count
- Consider using `--fields minimal` for faster response times with large result sets

## Message Size Limits

| Scenario | Limit | Notes |
|----------|-------|-------|
| Single message content | 1MB | Larger messages indexed but may be truncated in display |
| Messages per conversation | 10,000 | Practical limit for search performance |
| Total message count | 1,000,000+ | Tested with streaming indexer |

### Content Handling

- Messages over 1MB: Indexed fully, but TUI display may truncate
- Very long lines (>10,000 chars): Wrapped in display
- Binary content: Skipped during indexing

## Codex Source-File Admission (GH #489)

Modern lowercase `.jsonl` rollouts default to a **104857600-byte (100 MiB)**
source limit. Larger complete histories can be admitted explicitly, for example
with a 512 MiB budget:

```bash
CASS_CODEX_MAX_SOURCE_BYTES=536870912 cass index --json
```

The value is a decimal integer byte count from **1 through 1073741824 (1 GiB)**.
Whitespace around the value is accepted; zero, negative values, unit suffixes,
empty values and overflow are errors, not an unlimited mode. Unset the variable
to restore the default. Configuration is read once before each Codex scan and
remains fixed across its source attempts and enrichment passes. A larger budget
does not require `--full`: the normal incremental path retries excluded history
under the existing conservative watermark policy.

This override is specific to modern `.jsonl` rollouts. Legacy `.json` sources
retain the published FAD parser's 100 MiB ceiling (or the configured limit when
smaller). Raising only CASS's legacy limit would falsely certify files that the
upstream parser skipped. Other providers' limits are unaffected.

An over-budget source is not parsed into a partial conversation or reported as
complete. Healthy neighboring sources can still be ingested, but incomplete
coverage remains an error. The diagnostic's top-level `limit_bytes` is the
configured budget; a sampled rejected source additionally carries `limit_bytes`
when its format has a lower effective cap. Source bytes are never rewritten,
split or truncated to fit the limit. Observable source changes and unfinished
tails continue to prevent source completion, and consumer/storage failures
still stop the scan rather than being converted into ordinary source skips.

**This is an input-file admission limit, not an RSS ceiling or a wall-clock
deadline.** The primary parser retains normalized messages and runs before CASS
enrichment; the latter reads only the admitted snapshot's finite prefix. A larger
budget can therefore increase memory and CPU use. This setting does not
establish the older performance estimates elsewhere in this document, and does
not add a `--partial-ok` policy or suppress exit 9 for other incomplete scans.

## Memory Usage

| Operation | Expected Memory | Notes |
|-----------|-----------------|-------|
| Idle | ~50MB | Base application footprint |
| Search (10K docs) | ~100MB | Includes result caching |
| Search (100K docs) | ~200-300MB | May vary with result size |
| Full index rebuild | 500MB-1GB | Temporary spike during indexing |

### Memory Management

- LRU cache automatically evicts old entries
- Memory growth during search is bounded
- Explicit cleanup on index close

## Concurrent Operations

| Scenario | Tested Configuration | Performance |
|----------|---------------------|-------------|
| Parallel searches | 8 threads, 100 queries each | 100% success rate |
| Sustained load | 5 seconds continuous | Max latency <2s |
| High concurrency | 32 threads | 95%+ success rate |
| Search during indexing | Concurrent read/write | 90%+ search success |

### Thread Safety

- SearchClient is thread-safe (each thread should create its own instance)
- Index updates are atomic
- Reader reload is handled automatically

## Query Complexity Limits

| Query Type | Complexity | Expected Latency |
|------------|------------|------------------|
| Simple term | Low | <100ms |
| Prefix wildcard (`foo*`) | Low | <100ms (edge n-gram optimized) |
| Suffix wildcard (`*bar`) | Medium | <500ms |
| Substring (`*foo*`) | High | <1s |
| Boolean (AND/OR) | Medium | <500ms |
| Complex boolean | High | <2s |

### Query Recommendations

- Prefer prefix wildcards over suffix/substring when possible
- Use `--limit` to cap expensive queries
- Combine filters with queries to reduce search space

## Index Limits

| Metric | Limit | Notes |
|--------|-------|-------|
| Quill segment merges | The engine's in-commit tier merge is disabled (`tier_fanout = usize::MAX`); cass's capped planners consolidate | `cass index --full` consolidates a fragmented archive |
| Merge output size | 1 GiB per planned merge (estimate) | `CASS_LEXICAL_MERGE_MAX_OUTPUT_BYTES`; an oversized singleton is left unmerged; does not cap total RSS |
| Documents per merge run | 4,194,304 | `MAX_FOLD_OUTPUT_DOCS`, Quill's per-term posting limit |
| Query work per lexical search | 10,000,000 fuel units | `CASS_QUILL_QUERY_FUEL_BUDGET`; exhausted hybrid searches drop the lexical leg |
| Integrity preflight | Archives up to 2 GiB | `CASS_INDEX_INTEGRITY_PREFLIGHT_MAX_BYTES`; background runs skip the one-time migration repair above it |
| Schema changes | Trigger full rebuild | Versioned with hash |
| Concurrent indexers | 1 | `index-run.lock` admits one indexer; others exit 7 `index-busy` |
| Concurrent readers | Unlimited | Thread-safe |

## Network/Sync Limits (Remote Sources)

| Operation | Timeout | Notes |
|-----------|---------|-------|
| SSH connection | 10s | Configurable |
| rsync transfer | 300 s of I/O inactivity | rsync `--timeout`; no wall-clock limit on a transfer that keeps moving |
| SFTP fallback | Per-file | When rsync unavailable |

## Environment Variable Overrides

| Variable | Default | Purpose |
|----------|---------|---------|
| `CASS_CACHE_SHARD_CAP` | 256 | Max entries per cache shard |
| `CASS_CACHE_TOTAL_CAP` | 2048 | Total cache entry limit |
| `CASS_CACHE_BYTE_CAP` | available memory / 128, clamped to 64 MiB–2 GiB | Total cache byte limit; `0` disables the byte guard |
| `CASS_PARALLEL_SEARCH` | `true` | Boolean: parallel vector search on or off |
| `CASS_WARM_DEBOUNCE_MS` | 120 | Debounce for warm worker |
| `CASS_SEMANTIC_EMBEDDER` | auto | Force hash/ml embedder |
| `CASS_STREAMING_INDEX` | true | Enable streaming indexer |
| `CASS_CODEX_MAX_SOURCE_BYTES` | 104857600 | Modern Codex JSONL source-byte admission; 1..1073741824, legacy JSON at most 100 MiB |

## Tested Configurations

### Load Test Results (from P6.9)

```
Archive Size Tests:
  - 1K conversations: PASS (search <1s)
  - 10K conversations: PASS (search <5s)
  - 50K conversations: PASS (search <10s)

Message Size Tests:
  - Large messages (1MB): PASS
  - Many small messages (100/conv): PASS

Memory Tests:
  - Bounded search: <100MB growth over 500 searches
  - Resource cleanup: <50MB retained after test

Concurrent Tests:
  - 8 threads parallel: 100% success
  - Sustained 5s load: Max latency <2s
  - 32 thread stress: 95%+ success
```

## Known Limitations

1. **Single writer**: Only one process can write to the index at a time
2. **No incremental schema migration**: Schema changes require full rebuild
3. **Memory-mapped files**: Large indexes need sufficient virtual memory
4. **macOS keychain**: ChatGPT decryption only works on macOS

## Troubleshooting

### Slow Searches

1. Check index health: `cass health --json`
2. Rebuild if needed: `cass index --full`
3. Use `--limit` to cap results
4. Try `--fields minimal` for faster response

### High Memory Usage

1. Reduce `CASS_CACHE_TOTAL_CAP`
2. Set `CASS_CACHE_BYTE_CAP` to limit cache memory
3. Restart to clear accumulated state

### Index Corruption

1. Run `cass health --json` to diagnose
2. If only derived search assets are stale or corrupt, run the index refresh recommended by health
3. Run `cass doctor check --json` if the canonical SQLite archive is unreadable or malformed
4. Check disk space availability

## Version History

| Version | Changes |
|---------|---------|
| 0.1.57 | Initial load testing documentation |
