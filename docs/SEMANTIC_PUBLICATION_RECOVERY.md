# Ready-publication recovery admission (GH #490)

The recovery commands described in `SEMANTIC_ARTIFACT_RECOVERY.md` now also
admit ready legacy tier data before retiring scratch. Saving a ready record
and clearing its checkpoint is not, by itself, evidence that the published
vector file and any acknowledged WAL records are present and durable.

When candidate deletions exist, both ready tier records must open through the
native read-only FSVI reader, match their recorded producer and dimension,
and provide at least the recorded number of distinct live documents across
main and WAL. No model revision is guessed and no canonical corpus scan is
performed. Ordinary appends beyond the recorded count are allowed when their
visible vectors remain usable. Main/WAL overlap does not count as an additional
document. Main and WAL file handles, their named parents, and resolved alias
parents are synced before recovery can remove a candidate. Preview performs
admission without these durability syncs.

A valid header and matching identity count can still conceal unusable payloads.
For example, finite nonzero f32 input may become zero or infinity when persisted
as f16. Recovery therefore checks every effective live vector before admitting
its durability. Main rows use the native `is_vector_usable` check. WAL vectors
must have the recorded dimension and a finite, positive f32 squared norm.
Last-write-wins WAL replacements take precedence: tombstoned main rows and
main rows superseded by WAL are not treated as visible data. Validation does
not stop at the manifest's recorded count, since extra live appends also belong
to the selected publication.

A missing or unreadable ready publication, incompatible producer/dimension,
insufficient live coverage, unusable visible vector, nonregular sidecar, or
data-sync failure refuses explicit recovery and retains all candidates. JSON
commands use the existing `recovery_failed` diagnostic and exit 1; this is not
an empty successful plan. Nothing selects an orphan as a replacement or repairs
an index/WAL implicitly.

Automatic backfill treats this specific data-admission failure as deferred
cleanup, so it cannot disable the writer that needs to rebuild missing data.
Scratch remains until a later successful operation establishes usable durable
publication/checkpoint data. Stale-input and metadata validation remain strict.
A failed or unwinding rebuild does not authorize cleanup.

Ownership is still independent of readiness. Not-ready legacy and shard records,
ANN, and authenticated generation references retain all existing protection.
Ready shards use the same per-file admission, as described in
`SEMANTIC_SHARD_RECOVERY.md`; none of these checks promotes an artifact.
No-candidate sweeps skip the extra data admission. Cardinality admission retains
a fallibly grown set of borrowed IDs up to the recorded count. Payload admission
additionally reads the visible main vector slab and replayed WAL and retains
borrowed WAL IDs to resolve replacements; it does not clone the vector corpus.
Consequently, recovery with candidates can perform I/O proportional to the
published vector payload. This is not a proof of canonical identity/content
correspondence, complete file integrity, model-revision compatibility, or query
readiness.

## Resumable checkpoint payloads

With candidate deletions present, the checkpoint now passes the same effective
live-vector check after its existing producer and distinct-count admission.
A checkpoint can have enough identities while quantization has lost its vector
signal. Such a checkpoint cannot authorize deleting usable fallback copies.
Tombstones and native WAL replacement precedence are respected, including a
valid WAL replacement of an unusable main row not yet tombstoned after a crash.
Extra effective live rows ahead of the saved cursor must also have usable data.

Unlike the typed unavailable-publication error, an invalid checkpoint remains
a startup refusal: the engine must not continue its old cursor over bad data.
Preview/apply refuse reclamation and post-success cleanup defers. Recovery does
not repair data, choose a fallback, compact a WAL, or rewrite the cursor.
Explicit restoration of valid checkpoint data permits a new preview and resume.
These checks can read the live checkpoint payload; they are not metadata-only
or a complete canonical-content audit. No-candidate behavior is unchanged.

Focused regressions (authored; execution requires a Rust-enabled checkout):

```sh
cargo test --locked --lib indexer::semantic::artifacts::publication::
cargo test --locked --lib indexer::semantic::artifacts::checkpoint_tests_live::payload
cargo test --locked --lib indexer::semantic::artifacts::inspection::tests
cargo test --locked --lib indexer::semantic::artifact_lifecycle_tests
```

The existing alias-ownership regression keeps its ready record, but replaces
invalid placeholder data with a real FSVI/WAL pair and adds byte-preservation
assertions. Payload regressions cover f16 signal loss/overflow in both tiers,
invalid data beyond the recorded count, effective WAL replacement precedence,
tombstones, refusal before data pinning, and explicit repair. Negative tests
require unusable ready data to retain fallbacks; no previous ownership assertion
is removed. The five checkpoint-payload regressions also retain live bytes,
existing/fresh reader results, and the unchanged resume cursor on refusal.
