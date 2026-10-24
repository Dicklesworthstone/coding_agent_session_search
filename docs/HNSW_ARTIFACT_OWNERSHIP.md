# HNSW maintenance and semantic artifact ownership

The public `SemanticIndexer::build_hnsw_index` entry point takes the same
artifact lease as CASS backfills, full rebuilds, canonical replacements,
appends and shard publication. The native graph save lock only serializes
its own files; it does not exclude CASS recovery or other vector writers.
The outer lease now covers both native-current graph verification and new
graph construction through save/readback.

This is lock-only acquisition. It does not reclaim scratch, read a manifest,
promote readiness or delete old graph generations. A caller may still supply
a retained staging reader or construct a graph in a new output directory.
Recovery cannot collect that source in the same archive while this call owns
the lease. A retained reader from another archive still requires that source
archive's caller-owned coordination; the output lease cannot protect both.
The caller must retain its canonical maintenance lock across source selection,
this call, and any subsequent HNSW manifest update. Acquiring this lease does
not establish that an earlier-selected reader is the current publication.

Cancellation is checked before and immediately after lease acquisition. The
native graph build/save is unchanged and is not made interruptible by this
wrapper. A successful native publication is not reported as a cancellation
because a signal arrived afterwards. Error and unwind paths release the lease,
without authorizing cleanup. Existing rejection of incompatible producers,
revisions, dimensions, and pending WAL coverage remains in the native path.

The source-dependent native publisher, graph formats, old-generation retention,
and GC eligibility rules are unchanged. In particular, historical
`.hnsw-*.generation-*` directories are **not** new recovery candidates.

The five new file-backed tests use actual hash-produced FSVI and native HNSW
construction. They cover contention for missing and existing graphs, exclusion
of recovery and the public vector writers before native work, cancellation and
unwind release, native contract errors, byte-preserving retries, a retained
staging source with invalid ledger metadata, and a new graph-only output root.

```sh
cargo test --locked --lib indexer::semantic::delegate::full_rebuild_tests::hnsw
```

The existing recovery workflow's full-rebuild test filter includes this module.
These tests require Rust/native execution; source review is not a passing run.
