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
one row at a time; the known derived `fts_messages` virtual table and its shadow
tables, and SQLite internal tables, are omitted. Unknown virtual tables and
unkeyed tables are refused rather than silently losing data. Canonical schemas
with unsupported identifiers, key types, or oversized rows require an explicit
format extension; they are not truncated.

The destination uses a separate adjacent lock, with a five-second lock
acquisition deadline. An export is written to a private temporary file in the
same directory, flushed, synced, and independently reread through the verifier
before no-clobber publication. An existing output is never replaced. Persistent
lock files prevent competing processes from locking different inodes. These
controls do not impose a wall-clock deadline on database opening or scanning.

## Restore into a new canonical database

Import requires `--archive-id` to match the input header and `--include-private`
to acknowledge the private data it writes. `--output` is a **new database file**
whose parent already exists, not a live archive to overwrite. Existing database
files, links and SQLite `-wal`, `-shm` or `-journal` sidecars are refused.

The importer opens one regular, non-symlink input file and validates the header
before initializing a private candidate. Only this binary's canonical storage
initializer supplies executable schema. The storage version and every table,
column and primary-key descriptor must match exactly; a matching version number
alone is not sufficient. Unknown, additional or omitted tables fail explicitly.
There is no cross-schema migration or execution of SQL from the interchange file.

Rows are individually bound as typed SQL parameters. No exported path is used as
a write destination and no URL or provider source is fetched. Initializer seeds
are removed only from the new private candidate. Trusted initializer triggers
are suspended during replay and reinstated afterward, avoiding duplicate derived
writes. Input batches are limited to 128 records or 16 MiB of consumed JSONL,
including whitespace, whichever comes first. Batch commits are never exposed as
a valid partial restore: the complete stream, footer, counts, digest, canonical
schema metadata, foreign keys and database integrity must all pass first.

After closing the writer, import reopens the persisted candidate read-only and
re-exports its actual typed rows to a digest sink. Its digest must equal the input
completion. This catches storage-affinity conversions, missing writes and schema
side effects that input-only verification would miss. Publication requires a
single database independent of SQLite sidecars. A synced same-filesystem hard
link creates the new destination atomically without replacement; unsupported
filesystems fail instead of falling back to an overwriting copy or rename.
Unix output permissions are private (0600), and the parent directory is synced.

The input, source archive, existing destinations and provider histories remain
untouched. This does not install lexical or semantic search assets: those must
be rebuilt separately. The receipt reports `omitted_rebuild_required` rather than
claiming that search indexes are already usable. Inspection and restore do not
start model acquisition, provider scans or detached maintenance.

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
schema. Merge, existing-destination reimport, cross-schema migration and automatic
search-index rebuild are not implemented by this slice. It does not close bead
`.34`. The ordinary library command parser, root help, completion generation and
robot capabilities are not yet extended; `cass archive --help` documents the
binary's explicit archive frontend. Search and existing commands retain their path.

Rust regressions cover typed rows, cross-table relationships, trigger suspension,
schema disagreement, bounded batches, provenance, truncation and tampering,
source preservation, existing-output/sidecar protection and symlink refusal.
Native compilation, these tests, RCH/Clippy/UBS, large-archive bounds and platform
acceptance must be executed before release qualification. Source tests are not
passing-test receipts.
