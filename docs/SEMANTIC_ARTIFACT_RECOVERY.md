# Semantic backfill scratch recovery (GH #490)

`cass-semantic-reclaim` is a companion Cargo binary for inspecting and reclaiming
interrupted semantic-backfill scratch without starting another rebuild. It uses
exactly the same ownership classifier as automatic backfill cleanup. It is not a
new `cass storage` or `cass doctor` subcommand. Build this companion explicitly;
this change does not add it to release archives or installer workflows.

## Build and inspect

Build from a checkout containing the recovery implementation:

```sh
cargo build --locked --release --bin cass-semantic-reclaim
```

Stop active index and backfill writers, then preview the existing archive:

```sh
./target/release/cass-semantic-reclaim \
  --data-dir "/path/to/cass-data" --json
```

Preview is the default; `--dry-run` is an equivalent explicit spelling. The data
directory is mandatory: this command never guesses which archive to clean or
creates a missing archive/vector root. It can create its two lock files, but it
does not rewrite manifests, checkpoints, vector files, or scratch artifacts.
Neither the canonical database nor an embedding model is opened.

The JSON object has `operation: "preview"`, `status: "ready"` or `"blocked"`,
and a `plan` containing candidate paths, file/directory counts, logical byte
sizes, and `plan_fingerprint`. Candidate paths are relative to the reported
canonical data directory. A missing resumable checkpoint produces a blocked
preview, not a successful zero-byte cleanup recommendation.

## Apply an inspected plan

Copy the exact `plan.plan_fingerprint` from the preview. Apply is a separate,
explicit invocation against the same archive:

```sh
./target/release/cass-semantic-reclaim \
  --data-dir "/path/to/cass-data" \
  --apply --plan-fingerprint "<64-character-plan-fingerprint>" --json
```

The placeholder above must be replaced, not passed literally. Apply without a
fingerprint, a fingerprint without `--apply`, and `--apply --dry-run` are usage
errors. There is no force bypass.

Apply reacquires `index-run.lock`, then `semantic-backfill-artifacts.lock`, and
rediscovers ownership and candidates. It makes the observed manifest and active
checkpoint durable before any removal. The approval binds the archive, metadata
contents, resolved protected paths, and candidate filesystem identities. A
changed plan rejects the whole request before any deletion: preview again and
inspect the changed inventory. Caller-supplied paths never authorize deletion.

The successful apply object has `operation: "apply"`, `status: "complete"`, and
a `report` with removed counts and bytes. A partially removed scratch directory
or another removal failure is reported under `failed_paths` with `status:
"partial"` and a nonzero exit code. Cleanup is not transactional: some entries
may already have been removed when another removal fails. Re-preview before a
retry. Result-output failures also do not imply that an apply did not run.

## What is eligible, and what is retained

Only recognizable root-level `.staging-fast-*.fsvi` or
`.staging-quality-*.fsvi` files, their matching FSVI WALs, and
`.backfill-reuse-*` scratch directories are eligible. The staging name must match
the production tier/embedder/fingerprint grammar. A name alone is never enough:
both legacy tier records, HNSW records, the resumable checkpoint, shard metadata,
and the authenticated current generation protect referenced paths and resolved
aliases, including references marked not ready. Main/WAL ownership is protected
as a pair.

Canonical index names, generation directories, shard directories, quarantine,
and unrelated files are not swept. Candidate symlinks and Windows reparse
points are not followed. Links inside an otherwise orphaned scratch directory
are treated as leaves, never as permission to traverse their targets. Corrupt,
unsupported, or unreadable authority metadata prevents reclamation; missing
checkpoint data retains potential fallback artifacts.

When candidates exist and the manifest names a resumable checkpoint, recovery
also requires that checkpoint to open through the engine's read-only FSVI reader
with the recorded producer ID. Its physical main slots plus replayable WAL
records must also cover the checkpoint's recorded document count. This rejects
lost acknowledged WAL records without rejecting a valid append ahead of the last
checkpoint. An empty, truncated, unreadable, underfilled, or wrong-producer
checkpoint cannot authorize deleting older copies. Preview, apply, and automatic
startup cleanup share this check. Inspection never compacts or repairs its WAL,
and does not select an orphan as a replacement. Resolve the checkpoint problem
before requesting a new cleanup approval; a failed check retains the candidates.
This is structural reader admission, not a full vector-content or corpus-coverage
audit. Existing published indexes remain protected regardless of readiness.

Counts describe fully removed top-level staging files/WALs and reuse
directories. Byte figures are logical regular-file lengths, not allocated disk
blocks or a guarantee of immediately freed physical space. A failed partial
removal is not included in the reclaimed-byte total. Inspection walks scratch
metadata with an entry budget; approval fingerprints do not hash vector payloads.
The separate checkpoint-reader admission above may read the checkpoint's WAL;
it is skipped when there are no candidate deletions or no active checkpoint.

The locks serialize cooperating CASS writers. This is not a sandbox against an
uncooperative same-user process changing the filesystem during recovery. The
approval is an ownership/identity snapshot, not a vector-integrity or semantic
readiness attestation. On Unix, inode/device and ctime also detect a same-length
rewrite whose mtime was restored; other platforms use their available metadata.

## Exit codes and diagnostics

| Exit | Meaning |
| --- | --- |
| 0 | Preview is ready, apply completed, or help/version was displayed. |
| 1 | Recovery or output failed, including a busy lock, stale approval, invalid metadata, or an unusable checkpoint. Inspect the diagnostic; an I/O failure may follow partial removal. |
| 2 | Invalid command-line arguments; recovery was not started. |
| 3 | Preview is blocked by a missing checkpoint, or apply reports incomplete removals. |

With `--json`, successful/blocked/partial result objects go to stdout, while
usage/recovery/output errors go to stderr as an `error` object. Help/version
remain ordinary text. Without `--json`, results are human-readable.

## Focused qualification

The library and command tests use real temporary files and hash-produced FSVI
indexes. They require no model download. Run them in a Rust-enabled checkout:

```sh
cargo test --locked --lib indexer::semantic::artifacts::inspection::tests
cargo test --locked --lib indexer::semantic::artifact_lifecycle_tests
cargo test --locked --lib indexer::semantic::artifacts::checkpoint_tests
cargo test --locked --lib indexer::semantic::artifacts::storage_tests
cargo test --locked --bin cass-semantic-reclaim
cargo test --locked --test semantic_artifact_reclaim_cli
```

Coverage includes stale approvals, ownership changes, active locks, malformed
metadata, missing/damaged checkpoints, wrong producers, WAL-backed resume,
changed scratch, symlink targets, live-index
bytes and fresh search reopen, plus separate-process command execution with an
invalid canonical database. These are qualification commands, not a claim that
the full CASS dependency graph or platform matrix has passed.

The storage-backed lifecycle suite also runs capped quality-tier passes with
the deterministic hash producer against a real FrankenSQLite archive. It covers
content-fingerprint rollover after ingest, appended turns in covered conversations,
vector reuse, same-fingerprint resume, cleanup after a rejected reuse candidate,
and the completed-cache fast path. This exercises the public cleanup wrapper,
not only the inner embedding engine. It does not qualify native model quality.
