# Unchanged semantic backfills after a cache miss

A completed-build cache is a performance hint, not a reason to replace an
otherwise current publication. The public storage-backfill facade now performs
an additional read-only proof when that cache is absent, corrupt, or stale.
This implements the zero-delta portion of the GH #458 rollover contract and
avoids creating another staging/reuse copy during GH #490 recovery when the
published index already covers the archive.

## What the proof requires

The optimization is limited to a ready legacy F16 publication with no active
checkpoint or immutable `current.json` selection. Tier, producer, model revision,
current vector-space/input revision, schema, dimension, path, size, and document
count must match. A pending WAL, tombstone, duplicate identity, non-finite or
zero-norm vector prevents the no-op.

The proof streams the existing canonical passage projection and compares every
exact document ID, including its content hash and filter/provenance fields,
against the retained read-only vector image. It separately checks conversation
coverage. Same-ID edits, role/provenance changes, deletions, and additions cannot
be hidden by an unchanged coarse count/tail fingerprint or by an embedding count
of zero.

The archive observation is tied to the actual open main-file descriptor and its
real WAL path. Before/after observations include inode/device and ctime, so a
restored mtime cannot hide a rewrite. An already-pinned SQL transaction or an
archive pathname replaced since the connection opened cannot certify a no-op.
A change during proof returns an error before publication or cache refresh.
This additional optimization is Unix-only; other platforms retain their
existing reconciliation path.

## What remains unchanged

On success, backfill reports `unchanged: true`, `published: true`, zero newly
embedded documents, and no checkpoint save. Here `published` describes the
retained complete artifact; it does not mean a new publication occurred.
`unchanged` does not mean that no read-only validation scan occurred.

Vector and graph bytes, published completion time, tier/ANN records, and the
durable manifest remain unchanged. The proof does not invoke embedding, create
staging/reuse copies, run the publisher, restore revoked readiness, or certify
an unknown producer. Existing serving admission remains authoritative; this is
not a model-conformance certificate or an ANN repair operation.

The original artifact lease, stale-manifest rejection, and safe startup cleanup
still surround the operation. Therefore an unchanged pass can reclaim abandoned
scratch without rebuilding or republishing the live index. Actual content or
contract changes continue through the existing strict rebuild path. This does
not implement continuous old-contract serving during a genuinely changed build
or close all of GH #458.

## Performance and cache refresh

A cold proof reads the vector payload once and replays canonical text in bounded
batches. Its identity set borrows IDs from the retained image rather than copying
all message text or all vector data. It is not a constant-time cold check.

A successful proof atomically refreshes only the existing version-1
`.completed-backfill-<tier>-<embedder>.json` skip receipt. The receipt contains the
observations actually proved, not fresh stamps taken after proof; subsequent
changes invalidate it under the engine's existing cache reader. Later unchanged
passes can use that metadata-only cache path. The regression suite explicitly
checks the existing reader accepts the refreshed receipt and rejects it after a
WAL-only canonical edit.

Cache-write failure is best effort: it neither invalidates a successful proof
nor withdraws the live index. The next pass repeats read-only proof. Losing the
advisory cache after a crash cannot change serving authority.

## Focused regression command

```sh
cargo test --locked --lib indexer::semantic::unchanged::tests
```

Tests use real FrankenSQLite archives, hash-produced FSVI files and real HNSW
artifacts; they do not qualify native-model relevance. Coverage includes both
capped and uncapped public backfills, missing/corrupt/stale caches, exact fresh
and existing-reader search results, real canonical deltas, interrupted scratch,
unready and WAL-bearing artifacts, cache-write failure, old SQL snapshots,
replaced archive paths, restored mtimes, foreign revisions and tombstones.
