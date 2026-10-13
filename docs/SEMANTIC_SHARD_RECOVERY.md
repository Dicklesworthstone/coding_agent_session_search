# Ready-shard recovery admission (GH #490)

The existing `cass semantic-reclaim` and automatic backfill recovery now admit
ready shard data before deleting fallback staging files or reuse directories.
This also applies to a shard-only archive with no legacy semantic manifest.

Discovery retains the exact bounded `semantic_shards.json` snapshot that supplied
its protected paths. When there are deletion candidates, every shard marked
`ready` must open through the native read-only FSVI reader, match its recorded
producer and dimension, and cover its recorded distinct live document count
across main and replayable WAL. Main/WAL overlap counts once; extra appended
records are allowed. The same data-admission implementation serves legacy tiers
and shards, including the existing effective-live-vector usability check.
Actual recovery syncs the files and their named/resolved parent directories
before deleting candidates; preview validates without these syncs.

One missing, truncated, incompatible, underfilled or unusable ready shard refuses
the entire sweep, not just deletion of a similarly named fallback. The existing
`PublicationUnavailable` type defers automatic startup cleanup so explicit
rebuilding remains possible with all fallbacks retained. Invalid or unsupported
sidecar metadata still refuses startup; it is not a repairable data error.

Not-ready shards retain ownership without requiring valid data. Optional ANN
files and selected immutable-generation artifacts retain their existing
protection; this addition does not audit them, decide which generation should
serve queries, or reclaim historical generation directories. Ready shards in
incomplete groups are checked individually because they still acknowledge data.
This is per-shard structural/cardinality and effective-vector usability admission,
not a proof of canonical coverage, cross-shard disjointness, or authenticated
vector-content integrity.

Checks open one shard at a time. The temporary distinct-ID set grows fallibly
up to that shard's recorded count; the payload checker also tracks WAL identities.
Unlike the original pending patch, the shared checker now reads effective live
main vectors as well as IDs and WAL vectors. It respects WAL replacement
precedence and skips tombstones, but its I/O cost can scale with the live vector
payload. A no-candidate sweep skips these data checks.

Focused qualification, using real hash-produced shards and temporary files:

```sh
cargo test --locked --lib indexer::semantic::artifacts::publication::tests
cargo test --locked --lib indexer::semantic::artifact_lifecycle_tests
```

The existing publication test filter includes `tests_shards`. Eight new tests
cover both tiers, missing/broken data, lost WAL, restoration, sync refusal,
not-ready ownership, optional ANN absence, strict metadata handling, the
no-candidate path, and sufficient IDs with unusable F16 signal. The tests are
authored; no local Rust execution is claimed. Existing vector-admission tests
and helpers are retained unchanged. The storage-backfill double-copy path is
unchanged by this fix.
