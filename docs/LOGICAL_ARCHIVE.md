# Logical archive export, verification and restoration

Implementation slices for `coding_agent_session_search-2l1b0.34`.

```sh
cass archive export --db /path/to/agent_search.db \
  --archive-id workstation-history --include-private --output history.jsonl
cass archive verify history.jsonl
cass archive import history.jsonl --archive-id workstation-history \
  --include-private --output /existing/private/directory/restored.db
```

`--data-dir` (or `CASS_DATA_DIR`) can supply the export source directory instead
of `--db`. Export requires an explicit source, a stable caller-assigned archive
identity, acknowledgement of private content, and a **new** output path. Reuse
that identity for subsequent exports of the same archive. Do not use a pathname
as identity. Receipts are JSON on stdout; failures are JSON on stderr.

Export opens FrankenSQLite read-only and holds one transaction across schema
inspection and all table scans. It does not migrate, repair, checkpoint, acquire
models, or update source export metadata. Physical logical tables are streamed
one row at a time; the known derived `fts_messages` virtual table and its exact
FTS5 shadow names, and SQLite internal tables, are omitted. Other prefix-sharing
tables are not discarded. Unknown virtual tables and unkeyed tables are refused
rather than silently losing data. Canonical schemas with unsupported identifiers,
key types, or oversized rows require an explicit format extension; they are not
truncated.

The destination uses a separate adjacent lock, with a five-second lock
acquisition deadline. An export is written to a private temporary file in the
same directory, flushed, synced, and independently reread through the verifier
before no-clobber publication. An existing output is never replaced. Persistent
lock files prevent competing processes from locking different inodes. These
controls do not impose a wall-clock deadline on database opening or scanning.
Offline verification rejects special files and symlinks before reading JSONL.

## Restore into a new canonical database

Import requires `--archive-id` to match the input header and `--include-private`
to acknowledge the private data it writes. `--output` is a **new database file**
whose parent already exists, not a live archive to overwrite. Existing database
files, links and SQLite `-wal`, `-shm` or `-journal` sidecars are refused by default.

The importer opens one regular, non-symlink input file and validates the header
before initializing a private replay database. Only this binary's canonical
storage initializer supplies executable schema. The storage version and every
table, column and primary-key descriptor must match exactly; a matching version
number alone is not sufficient. Unknown, additional or omitted tables fail
explicitly. There is no cross-schema migration or execution of SQL from input.

Rows are individually bound as typed SQL parameters using one prepared INSERT
per table. No exported path is used as a write destination and no URL or provider
source is fetched. Initializer seeds are removed only from the new private
replay database. Trusted initializer triggers are suspended during replay and
reinstated afterward, avoiding duplicate derived writes. Input batches are
limited to 128 records or 16 MiB of consumed JSONL, including whitespace,
whichever comes first. Batch commits are never exposed as a valid partial
restore: the complete stream, footer, counts, digest, canonical schema metadata,
foreign keys and database integrity must all pass first.

Replay retains the canonical WAL writer policy. A journal-mode change or clean
close does not by itself prove a self-contained database. After all private
batches commit and validate, parameterized `VACUUM INTO` materializes a separate
publication image containing the committed logical database. The replay files
are never relabelled as a complete image, and their sidecars are not discarded
to manufacture a successful check. Allow disk space for both the replay database
and its sidecars and the separate publication image during restoration.

Import rejects content-bearing sidecars beside the image, closes the writer,
then reopens that image read-only. It checks persisted foreign keys and integrity
and re-exports the actual typed rows to a digest sink. Its digest must equal the
input completion. This catches storage-affinity conversions, missing writes and
schema side effects that input-only verification would miss. Sidecar absence is
checked again after closing the reader. Only this synced image is published,
using a same-filesystem hard link that cannot replace an existing destination;
unsupported filesystems fail instead of falling back to an overwriting copy or
rename. Unix output permissions are private (0600), and the parent directory is
synced. A failure syncing that directory after publication is reported explicitly;
a visible destination after that failure is not a confirmed durability receipt.

The input, source archive, existing destinations and provider histories remain
untouched. By default import installs no lexical or semantic assets and reports
`derived_search_assets: "omitted_rebuild_required"`. It does not start model
acquisition, provider scans or detached maintenance. The explicit indexed-recovery
option below adds canonical-only lexical reconstruction; export and verification
never invoke that option.

## Recover a searchable profile without the original provider files

Add `--rebuild-index` to reconstruct lexical search immediately after the verified
canonical restore. The output must use the ordinary profile layout, and the
interchange input must be outside that profile:

```sh
mkdir -p /private/recovered-cass
cass archive import /private/backups/history.jsonl \
  --archive-id workstation-history --include-private \
  --output /private/recovered-cass/agent_search.db --rebuild-index
cass search "authentication" --data-dir /private/recovered-cass \
  --mode lexical --robot --no-maintenance
```

The destination directory must already exist. Its final component, and any
existing components of its lexical index path, cannot be symlinks. A filename
other than `agent_search.db` is rejected **before** restoration: the normal
`--data-dir` commands must not silently look for a different database. Keep the
input outside the destination directory because index, lock and checkpoint
names there belong to maintenance, not interchange storage. The output path,
not an ambient `CASS_DATA_DIR` or export-source `--db`, selects this profile.

This path reads only the restored canonical database. It does not run normal
provider discovery, rescan local histories, salvage historical source bundles,
create a semantic index, or acquire a model. It reuses the existing exclusive
index-run lock and the canonical scratch-build, checkpoint and atomic-publication
pipeline; it does not implement a second publisher. The index contains the
canonical search projection, while the database remains the full-fidelity
source of truth. Index document counts can differ from total stored rows.

On success the existing canonical digest, counts and `destination_status` remain
in the receipt. This opt-in additionally reports:

```json
{
  "derived_search_assets": "lexical_rebuilt_semantic_not_built",
  "lexical_rebuild": {
    "data_dir": "/private/recovered-cass",
    "index_path": "/private/recovered-cass/index/v9-quill",
    "indexed_documents": 4,
    "source": "canonical_archive",
    "provider_scan_performed": false,
    "semantic_assets_built": false
  }
}
```

The path and count above are illustrative; use the returned `index_path` rather
than hard-coding a schema-version directory. This is a lexical publication
receipt, not proof of semantic readiness or a lease against subsequent archive
writers. Canonical verification and lexical rebuilding are successive operations,
not one transaction spanning the database and search index. Run recovery in a
dedicated profile rather than alongside active ingest into that same profile.

The example search uses `--no-maintenance` to demonstrate that the import already
built a usable index, rather than allowing search to repair it implicitly.
Search results' exact source, conversation and message coordinates can then be
passed to the canonical follow-up commands below, even when the original provider
files and the original archive are unavailable.

### When canonical restoration succeeds but indexing fails

A disk/lock/publication error during lexical rebuilding makes the command fail,
with no success receipt. The already verified canonical database is **retained**;
it is not undone, deleted or overwritten. Read known conversations directly, or
resolve the indexing obstruction and repeat:

```sh
cass archive import /private/backups/history.jsonl \
  --archive-id workstation-history --include-private \
  --output /private/recovered-cass/agent_search.db \
  --if-identical --rebuild-index
```

`--if-identical` must verify the whole existing database before granting rebuild
authority. A different archive is a conflict even when its caller-assigned
archive ID matches; its import does not replace the existing database or rebuild
its search index. A successful retry reports `destination_status: "unchanged"`
for the canonical database while permitting writes to **derived** lexical assets.
Thus the combined flags are not a whole-directory read-only operation. Repeating
without `--if-identical` still refuses an existing database.

## Reading a recovered conversation

For a known conversation, canonical `view` and `expand` do not need the original
provider file or a search index. Exact-schema restoration preserves conversation
IDs, source IDs, source paths, and stored message indices. Select the recovered
database explicitly and use the canonical coordinate, not a physical file line:

```sh
cass --db /existing/private/directory/restored.db \
  view /original/provider/session.jsonl --source remote-host \
  --conversation-id 42 --message-index 8 -C 2 --json
```

The IDs and index above are examples; use the identities from the archive. A
one-based `--message-index 8` selects stored `messages.idx = 7`, not the eighth
physical line and not the eighth row in a sparse conversation. The same selectors
work with `expand`. Explicit source and conversation identity disambiguate
histories from different machines that use the same source pathname.

## Repeating an import safely

Without `--rebuild-index`, add `--if-identical` to permit an existing destination
**only as a read-only no-op**. It is not an overwrite, merge, repair or upgrade
flag:

```sh
cass archive import history.jsonl --archive-id workstation-history \
  --include-private --output /existing/private/directory/restored.db --if-identical
```

The complete input must validate before the existing database is opened. Under
the destination lock, comparison uses one read-only canonical snapshot, hashes
all logical descriptors and typed rows, checks foreign keys and integrity, and
requires the opened database's descriptor identity to match the destination
pathname before and after the comparison. No WAL checkpoint, schema migration,
import metadata or canonical write occurs on this path. Different content is a
reported conflict, never an upsert. Symlinks and unprovable file identities fail.

Import receipts add `destination_status: "created"` or `"unchanged"`; existing
export and verify receipt fields are unchanged. An unchanged receipt describes
the snapshot examined, not a lease excluding ordinary index writers afterward.
The source archive identity remains an explicit caller assertion matched against
the input header; the checksum is not external proof of identity or authenticity.
A missing destination still goes through the full private-candidate restore.
An interrupted pre-publication import can be retried from the original JSONL;
a later attempt does not trust or promote leftover private stages. Process-kill
recovery is distinct from proving power-loss durability on every filesystem.
After publication, a lost success receipt does not justify replacing the
destination: retry with `--if-identical` to validate its complete contents and
report `unchanged`. A different or incomplete destination remains a conflict.

## Version 1 wire contract

Each UTF-8 JSONL record ends in a newline and is at most 8 MiB, **including** that
newline. The reader checks this while buffering; the writer checks during JSON
encoding. No entire conversation or archive is accumulated by the application.
The database engine, one decoded record, encoding buffers, bounded schema
metadata and base64 scratch still consume memory. Batch limits do not establish
a bounded total-process RSS claim; that requires native large-archive measurements.

Records use a `type` discriminator:

* `header`: nested `header` contains `format: "cass.logical_archive"`,
  `schema_version: 1`, archive identity, export timestamp, canonical storage
  schema version, record types, private-content flag, and explicit omissions.
* `table`: nested `table` contains a name, ordered column names, and primary-key
  column offsets. Table names are strictly increasing. SQL declarations are not
  executable input and are not included.
* `row`: `values` contains one tagged cell per declared column. Rows are strictly
  ordered by their declared primary key with binary collation. Duplicate or
  unordered identities are rejected using only the previous key, not an
  archive-sized identity set.
* `completion`: nested `completion` contains total row count, per-table row
  counts, and the canonical SHA-256 digest. A missing or mismatched completion,
  any trailing record, unknown version, malformed UTF-8/JSON, or unterminated
  final line fails verification.

Cells use `kind` and, except for `null`, `value`: `integer` is signed 64-bit;
`real` is 16 lowercase hex digits encoding finite IEEE-754 binary64 bits;
`text` is a JSON string; `blob` is standard padded base64. Primary keys admit
non-null integer, text, or blob cells and are limited to 64 KiB of retained key
material. Descriptors are bounded to 256 tables, 256 columns per table, and
128-byte ASCII identifiers.

The digest starts with `cass.logical_archive.v1\0`, followed by compact canonical
JSON records with their newlines. The header timestamp is normalized to zero
when hashing. All table and row records are hashed; the completion is not.
Serialization follows the Rust format structs' declared field order, not input
object key order. The digest binds archive identity, schema, descriptors, cell
values and omissions, but not export time or incidental JSON whitespace. This
is an integrity checksum, **not** a signature or proof of source authenticity.

## Scope and qualification

Current restoration supports a new database with the exact current canonical
schema, plus opt-in read-only comparison for an identical existing destination.
Explicit indexed restoration additionally rebuilds canonical lexical search.
Merge, cross-schema migration and semantic reconstruction remain outside these
slices; default restoration remains offline. These slices do not close bead
`.34`. The ordinary library command parser, root help, completion generation
and robot capabilities are not yet extended; `cass archive --help` documents
the binary's explicit archive frontend. Existing commands retain their path.

Rust regressions cover typed rows, cross-table relationships, trigger suspension,
schema disagreement, bounded batches, provenance, truncation and tampering,
source preservation, existing-output/sidecar protection and symlink refusal.
Publication tests include committed WAL data with a main-file-only negative
control and subprocesses killed on both sides of the atomic link, after image
validation/fsync but before any success receipt, followed by create-or-verify
retry. The ignored subprocess entry point is invoked by its non-ignored parent
test; it is not counted as a passing regression. A real-binary journey restores
260 messages from two remote providers and checks exact sparse `view`/`expand`
coordinates with the original archive and source path unavailable.

Indexed-recovery regressions additionally exercise ordinary maintenance-disabled
search followed by canonical view, empty profiles, conflicting imports with
unchanged prior index files, local-history isolation, and lexical rebuild
failure followed by an identical retry. Admission tests cover invalid layouts,
input/profile separation, corrupt/missing canonical files and path aliases.
Their source presence is not a native execution receipt.

Native Linux run `35675603482` at immutable source `6cefa248` completed both
workflow jobs successfully: 48 archive regressions, six archive CLI regressions,
15 canonical-service regressions, 20 bookmark tests and nine bookmark-CLI tests
passed. This supersedes the earlier partial run `35672185382` at `d504e6e` and
covers prepared replay, source-less follow-up and both process-kill boundaries.
It does **not** qualify the later opt-in indexed-recovery implementation.
The latter needs its own exact-source native result. No large-archive RSS,
cross-platform, power-loss, Clippy/UBS or full-release qualification follows from
these fixture results. The targeted workflow records immutable source, lockfile
and binary identities and rejects empty test-filter success.
