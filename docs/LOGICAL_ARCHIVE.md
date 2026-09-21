# Logical archive export and verification

Implementation slice for `coding_agent_session_search-2l1b0.34`.

```sh
cass archive export --db /path/to/agent_search.db \
  --archive-id workstation-history --include-private --output history.jsonl
cass archive verify history.jsonl
```

`--data-dir` (or `CASS_DATA_DIR`) can supply the source directory instead of
`--db`. Export requires an explicit source, a stable caller-assigned archive
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

## Version 1 wire contract

Each UTF-8 JSONL record ends in a newline and is at most 8 MiB, **including** that
newline. The reader checks this while buffering; the writer checks during JSON
encoding. No entire conversation or archive is accumulated by the application.
The database query planner, one decoded record, encoding buffers, and base64
scratch still consume memory. A bounded total-process RSS claim requires native
large-archive measurements and is not established by this implementation.

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

This first slice provides export and offline verification, not import, merge,
restore, or cross-schema migration. It does not close bead `.34`. The ordinary
library command parser, root help, completion generation and robot capabilities
are not yet extended; `cass archive --help` documents the binary's explicit
archive frontend. Search and all existing commands retain their existing path.

Rust tests cover framing, canonical digests, truncation, bounds, malformed input,
identity ordering, source preservation, no-clobber output, a canonical-schema
fixture and symlink refusal. Native compilation, these tests, RCH/Clippy/UBS,
large-archive bounds and platform acceptance must be executed before release
qualification. Source tests are not passing-test receipts.
