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

## Add-only maintenance for identified generations (GH #460 / of9i7)

`SemanticIndexer::append_hnsw_generation` provides the guarded native append
operation for callers that already retain both identity-complete FSVI v2 source
generations. It is an associated function: constructing an indexer or loading a
model is unnecessary. With the canonical maintenance lock already held:

```rust
use coding_agent_search::indexer::semantic::SemanticIndexer;
use frankensearch::index::HnswAppendDisposition;

let outcome = SemanticIndexer::append_hnsw_generation(
    &previous,
    &current,
    &data_dir,
    "hnsw-quality.chsw", // existing metadata file inside vector_index
    4096,                // positive maximum number of new graph insertions
    || Ok(()),           // replace with the caller's cancellation check
)?;
match outcome {
    HnswAppendDisposition::Appended { inserted } => {
        // A new native graph was saved and read back against current.
        // Publish the corresponding manifest under the retained maintenance lock.
    }
    HnswAppendDisposition::NoOp => {
        // The persisted native graph was already verified against current.
    }
    HnswAppendDisposition::FullRebuildRequired { reason } => {
        // No graph was published. Inspect the reason; do not mark HNSW ready.
        // A full rebuild or larger append budget needs a separate decision.
    }
}
```

The API acquires the existing-writer artifact lease without creating an archive
or sweeping scratch. It restricts the graph metadata name to one `.chsw` file,
refuses symlinks/non-files, and checks retained metadata identity again before
replacement. These checks supplement the caller's maintenance lock; they are
not a filesystem compare-and-swap against an uncoordinated external writer.

Both source readers must be log-free and carry their real v2 identities. The
native append engine verifies matching embedding/storage/producer identities,
a distinct generation, unique document IDs, no missing old documents, and
bit-identical old vectors. It updates source-row mappings after FSVI sorting and
inserts only new document IDs. Graph construction parameters are preserved.
Legacy sources, pending WALs, missing/incompatible native graphs, changed/deleted
old evidence, duplicate IDs and over-budget growth never trigger an implicit
full rebuild. Native I/O, decoding, topology and cancellation errors remain
errors rather than being hidden behind a rebuild decision.

A successful retry recognizes the graph already bound to `current`, without
rewriting it, even when the retained input pair's growth exceeds a newly lowered
insertion budget. Refusals, errors before save, and cancellation/unwind during
candidate preparation leave persisted graph generations unchanged. After an
acknowledged save there is no late cancellation check. A save/readback failure
can still leave an installed native generation, so callers must retry verification
rather than infer that nothing was published from an error alone.

The insertion budget is **not** a whole-operation work or memory limit. Native
admission and append validation still inspect complete source generations;
load/save still process the complete graph. Cancellation cannot preempt those
native calls. This API inherits the native loader's ownership constraints and
does not enable ANN in the resident search service.

### Integration boundary and validation

This adds a functioning library maintenance operation, not the complete ordinary
CLI solution to GH #460. Normal semantic writers still need the v2 migration,
prior-generation retention/selection, and command-level dispatch and receipts.
The API does not fabricate v2 identities for legacy files, select an old source
by filename, or update the semantic manifest on its own. `of9i7` remains open.

The regression module uses real CASS hash embeddings, real v2 FSVIs and native
graph save/load/search. It covers old/new result identities, idempotent retries,
insertion limits, changed/deleted/duplicate evidence, generation mismatches,
legacy sources, pending WALs, missing graphs, lease contention, cancellation,
unwind, conflicting publication, and path confinement. Unix cases also retain
readers after source renames and refuse symlinked graph metadata.

```sh
cargo test --locked --lib indexer::semantic::delegate::hnsw::incremental_tests
```

These regression cases require native Rust execution. Their presence is not a
passing receipt or an archive-scale performance measurement.
