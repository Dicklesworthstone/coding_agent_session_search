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

Current exports default to **wire format version 2**. This binary also reads, verifies
and restores version 1 backups with their original limits. Version 2 supports a
complete encoded logical row up to 256 MiB by splitting large rows into physical
JSONL frames of at most 8 MiB each. An older binary that supports only version 1
refuses version 2 backups, including those containing only small rows; use a
version 2 reader for new exports. The receipt's `schema_version` reports the
version of the archive actually exported or read. The header's separate
`storage_schema_version` identifies the canonical database schema. Use
`--format-version 1` when an older reader is required and every encoded row fits
within 8 MiB; an oversized row is refused without publishing a partial archive.
`--format-version 2` explicitly selects the default large-row transport.

Export opens FrankenSQLite read-only and holds one transaction across schema
inspection and all table scans. It does not migrate, repair, checkpoint, acquire
models, or update source export metadata. Physical logical tables are streamed
one row at a time; the known derived `fts_messages` virtual table and its exact
FTS5 shadow names, and SQLite internal tables, are omitted. Other prefix-sharing
tables are not discarded. Unknown virtual tables and unkeyed tables are refused
rather than silently losing data. Unsupported identifiers, key types, and rows
exceeding the 256 MiB encoded logical-row limit are refused explicitly. Export
never truncates them or substitutes references to external provider files.

The destination uses a separate adjacent lock, with a five-second lock
acquisition deadline. An export is written to a private temporary file in the
same directory, flushed, synced, and independently reread through the verifier
before no-clobber publication. An existing output is never replaced. Persistent
lock files prevent competing processes from locking different inodes. These
controls do not impose a wall-clock deadline on database opening or scanning.
Offline verification rejects special files and symlinks before reading JSONL.

### Diagnosing an export failure

Export errors identify the operation that failed. A row conversion, validation
or output-write failure identifies the logical table and its **one-based row
position in primary-key order**. When the table has a single integer primary key,
the diagnostic also includes that column and value, for example
`logical table messages, row 1025 (id=5128)`. A row position is not a message ID.
Text and BLOB keys, message bodies and metadata values are never included.

The reason appears in the JSON error's `message`, without requiring debug logs.
Database-read errors include the FrankenSQLite error class and numeric code,
plus the number of complete rows already written for that table. I/O errors
report the error kind and OS code when available. Database page numbers and
short-read byte counts may also be reported; arbitrary engine and I/O message
payloads are withheld because they can contain private data. The original typed
causes remain available to the exit-code classifier, so a busy database or a
failed read is not mislabeled as an unsupported logical record.

The current 256 MiB limit applies to **one complete encoded logical row**,
including all its cells, JSON framing, escaping, base64 expansion and the final
newline. Each physical frame remains bounded to 8 MiB. A collection can be much
larger than either limit; a raw text field below 256 MiB can still exceed the
logical-row limit when combined with the other cells and encoding overhead.
The early payload check is only a lower bound; the bounded encoder decides
whether the logical row actually fits. A row beyond that limit fails explicitly
with its table, row and limit. Export never skips or shortens it. SQLite TEXT
containing invalid UTF-8 is also refused instead of silently substituting
replacement characters in a purportedly lossless backup. Version 1 inputs retain
their original 8 MiB limit on a complete encoded row.

An error after substantial streaming can therefore identify a single
unsupported row. It does not make the already written prefix a valid backup.
Failures before publication discard the private stage and leave no new final
archive; the persistent destination lock file is expected. Verification must
succeed and the completion receipt must be emitted before treating an export
as successful.

## Search and read a backup without restoring it

When only a logical backup is available, `archive search` searches complete
stored message bodies without opening SQLite, building an index, or accessing
provider paths. The input is a `cass.logical_archive` JSONL file, not a raw
provider transcript or ordinary search-result JSONL:

```sh
cass archive search /private/backups/history.jsonl \
  --contains "authentication" --limit 25 --include-private
```

Matching is **case-sensitive literal substring matching**. There is no stemming,
regex, Boolean syntax, relevance ranking, model inference, or search-index
truncation. `--contains` accepts 1..1024 UTF-8 bytes; `--limit` accepts 1..100
matches, ordered by canonical message ID. `--conversation-id` optionally scopes
the search to one exact positive ID. Each hit includes source ID/path,
conversation ID, message ID, one-based stored message index, and a preview of at
most 256 characters around the first match. Preview and match offsets are UTF-8
**byte** offsets in the full stored body; a preview is not a complete message.

The response counts all matching messages in `matches`, even after filling the
page. When `has_more` is true, pass `next_cursor` as `--cursor` with the same
substring and conversation filter. Page size may change. `matches_after_cursor`
counts the remaining matching messages, including the returned page. Cursors
bind the archive content digest and search criteria: a different snapshot or
query is refused rather than silently mixing pages. The final page has a null
`next_cursor`. Cursors are bounded, versioned continuation data, not credentials
or signatures. Do not modify them or share private results indiscriminately.

Follow a hit with `archive view`. Supply its exact `message_id` and the response's
`content_sha256`; the digest is mandatory so the same numeric ID in a replacement
backup cannot silently identify a different message. This example captures and
uses both values without shell interpolation of private content:

```python
import json
import subprocess

backup = "/private/backups/history.jsonl"
page = json.loads(subprocess.check_output([
    "cass", "archive", "search", backup,
    "--contains", "authentication", "--limit", "1", "--include-private",
]))
if page["hits"]:
    subprocess.run([
        "cass", "archive", "view", backup,
        "--message-id", str(page["hits"][0]["message_id"]),
        "--content-sha256", page["content_sha256"],
        "--context", "2", "--include-private",
    ], check=True)
```

View returns complete message **text and role**, plus source/conversation/message
identity; it does not expand arbitrary provider metadata or execute stored tool
calls. Context is 0..20 actual messages on either side, default 2, sorted by stored
message index. Sparse indices and wire row order are not mistaken for message
adjacency. `more_before` and `more_after` report omitted neighbours. Complete
text across the selected window must fit 64 KiB, counting UTF-8 and embedded
NUL bytes. Oversized windows fail with no partial success; reduce context or
restore the archive for larger bodies. Requested bodies are never shortened.

Search verifies two complete passes through one admitted regular-file handle:
match selection and bounded source-identity resolution. View verifies three:
exact target, bounded neighbour selection, and complete-body hydration. Headers
and digests must agree across every pass, and no results are emitted until the
final completion validates. Corruption outside the displayed window still
fails. Selected orphan relationships or ambiguous view coordinates fail rather
than falling back to a similarly named session on another machine.

These are sequential scans, not indexed lookups: work grows with backup size
and each page scans again. Decoding and validation retain state proportional to
one logical row, whose canonical encoding is at most 256 MiB in version 2, plus
bounded frame, encoding and page/context buffers. Physical JSONL frames remain
at most 8 MiB, and the encoded-response ceiling remains 2 MiB. The 64 KiB complete
view window is unchanged even when an unselected row is larger. These are not
measured whole-process RSS or wall-clock guarantees. No model,
database, profile, index or provider file is created or opened by these commands.
They report `content_source: "logical_archive"`, not canonical-database access.
`integrity_verified` means the complete wire checksum/count/order contract
passed; `database_integrity_checked: false` explicitly excludes a whole-database
foreign-key or physical integrity audit. The checksum is not source authenticity.
Both commands require `--include-private` before displaying session content.

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
explicitly. SQL from input is never executed. The only cross-schema paths are the
reviewed storage-schema v20 and v21 -> v22 bridges, and only with
`--allow-compatible-schema` (see [Reviewed restoration into v22](#reviewed-restoration-into-v22)).

Rows are individually bound as typed SQL parameters using one prepared INSERT
per table. No exported path is used as a write destination and no URL or provider
source is fetched. Initializer seeds are removed only from the new private
replay database. Trusted initializer triggers are suspended during replay and
reinstated afterward, avoiding duplicate derived writes. A batch rolls over
before the next logical record would exceed 128 records or 16 MiB of consumed
JSONL, including whitespace and continuation framing. One logical row cannot be
split across database inserts: an admitted row larger than the byte budget is
processed in its own batch and may exceed 16 MiB. Its canonical encoding still
must fit the 256 MiB logical-row limit; continuation transport adds base64 and
framing overhead. A continuation group counts as one logical record. Batch
commits are never exposed as a valid partial restore: the complete stream,
footer, counts, digest, canonical schema metadata, foreign keys and database
integrity must all pass first.

Before publication or an identical-retry success, the migration ledger and
`meta.schema_version` must agree with the admitted storage version. Every active
initializer migration through that version must have a ledger entry: ordinary
storage opening replays a missing intermediate migration even when the latest
version is present. Such a replay can overwrite restored canonical state.
Fresh histories beginning at v13 and legacy histories retaining v1 through v12
are both accepted. Missing steps are reported by version without printing
historical migration names or message contents, and are never repaired during
backup restoration or comparison.

Restore failures retain their database cause and report the failed operation,
including batch boundaries and verification probes. An insertion failure also
identifies the logical record, table, one-based table row and single integer
primary key when available. These diagnostics exclude text/BLOB keys, message
bodies, constraint payloads and arbitrary engine messages. Database I/O and busy
failures retain their retryable classes. A failed integrity-query read supplies
no integrity verdict; it is reported as a failed probe rather than as proof of
broken relationships. The same policy applies to reviewed migrations and
existing-destination comparisons.

Replay retains the canonical WAL writer policy. After all private batches commit
and validate, the engine's verified page-backup API checkpoints the private
replay database and copies its committed image to a new publication candidate.
The pinned engine copies with a fixed 64 KiB buffer and hashes images one page
at a time. This avoids the whole-database hydration of its `VACUUM INTO` path.
Only the private replay database is checkpointed; the input and the user's live
source database are never passed to this API. The replay files are never
relabelled as a complete image, and their sidecars are not discarded to
manufacture a successful check. Allow disk space for both the replay database
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

## Reviewed restoration into v22

An archive exported by a build whose storage schema is v20 or v21 (v0.9.0 is
v21) fails an exact import into a v22 build. `--allow-compatible-schema`
admits those two reviewed bridges and nothing else. Any other version pair is
refused. What the bridges allow:
- v21 added only an index.
- v22 added only the `forgotten_sources` table (the `cass forget` tombstones).
  An older archive cannot carry it, so it is created empty, which is what an
  in-place upgrade does.
- Every other canonical table, column and primary-key descriptor must match
  the current ones. An archive that differs, or that already carries
  `forgotten_sources`, is refused.
- A later identical retry also refuses a destination whose
  `forgotten_sources` gained rows.

```sh
cass archive import history-v21.jsonl --archive-id workstation-history \
  --include-private --allow-compatible-schema \
  --output /existing/private/directory/restored.db
```

The current binary's initializer remains the only schema authority. Archived
`_schema_migrations` rows and `meta.schema_version` are verified as input but
not replayed. The receipt adds `schema_migration` with `mode`
(`"reviewed_v20_to_v22"` or `"reviewed_v21_to_v22"`), the from/to storage schema versions,
`schema_authority: "current_binary_initializer"` and `source_rows_verified`.
The flag combines with `--if-identical` (a retry reports `unchanged` without
modifying the database image) and with `--rebuild-index`, whose failure message
names the retry flags to repeat.

Each accepting pass requires the archived `meta.schema_version` to be text
matching the header's source storage version. A missing or conflicting marker
fails before a migration candidate is created. Persisted comparison validates
the same records it compares, including the shape, primary-key order and digest
of skipped migration-history rows. It must reach a valid completion matching
the originally inspected archive before issuing a success receipt. Rewinding
an already verified input is never treated as proof that its contents stayed
unchanged. This applies to both version 1 and version 2 archives and to
`--if-identical` retries.

## Exit codes

Archive failures are JSON on stderr with a kebab-case `kind`:

| Exit | `kind` | Retryable | Meaning |
|---|---|---|---|
| 2 | `logical-archive-usage` | no | Malformed or unacknowledged request |
| 5 | `logical-archive-integrity` | no | Archive failed decoding, count or digest checks |
| 7 | `logical-archive-busy` | yes | Destination/source locking, a transaction conflict, or an expired read snapshot prevented completion |
| 14 | `logical-archive-io` | yes | File, database-page, or checkpoint I/O failed |
| 9 | `logical-archive-error` | no | Anything else, including an occupied destination |

Retry a busy failure by running the whole archive command again. An expired
snapshot requires a new read transaction; an emitted prefix must never be
continued using a different snapshot. Corruption, constraint violations,
database capacity limits and ambiguous multi-process consistency failures stay
nonretryable. Engine error strings do not determine the classification.

## Version 2 wire contract

Version 2 exports have `schema_version: 2` and declare these `record_types`, in order:
`table`, `row`, `row_start`, `row_chunk`, `row_end`, `completion`. The header,
table descriptors, typed cells, primary-key order, and completion have the same
logical meanings as version 1. A row whose complete canonical JSON and newline
fit within 8 MiB is written as one ordinary `row` record with the same bytes as
version 1. The limit for an entire canonical logical row is 256 MiB.

A larger row is written as one contiguous continuation group:

* `row_start` declares `bytes`, the byte count of the complete encoded logical
  row including its final newline. It must exceed 8 MiB and fit within 256 MiB.
* `row_chunk` carries a zero-based `sequence` and standard padded base64 `data`.
  Each chunk decodes to 1 MiB of the row's canonical JSON bytes, except the last,
  which contains exactly the remaining bytes. Chunk boundaries can fall inside
  UTF-8 characters or JSON escapes; decoding reconstructs the original bytes.
* `row_end` carries the lowercase hexadecimal `sha256` of those complete row
  bytes. The count and checksum must both match before the reader returns a row.

Every physical frame is UTF-8 JSONL, ends in a newline, and fits within the same
8 MiB frame bound. Declared row size, chunk size and sequence are checked while
reading, with at most 256 chunks per row. Missing, duplicate, reordered, empty
or extra chunks, malformed base64, checksum mismatches and incomplete groups
fail verification. Only logical rows may use continuations. Padding a small row
with whitespace does not permit it to use a continuation group or bypass the
version 1 row limit.

Typed parsing enforces the 256-column limit before collecting additional cells.
Cell values are restricted to their scalar wire types during parsing, including
when the value precedes its kind tag. Arrays or objects cannot cause an untyped
JSON tree to be materialized as a malformed cell or continuation frame. Unknown,
duplicate and mismatched fields are rejected, and a continued record must be a
row before other record payloads are read. Valid field order remains flexible.

The encoder holds one bounded canonical row and emits chunks successively.
The decoder retains one decoded 1 MiB chunk while constructing the typed row;
it does not concatenate a second complete encoded input row. Archive input
validation counts and hashes canonical bytes through an 8 KiB buffer instead of
allocating another encoded row. BLOB validation uses a fixed 3 KiB decode buffer
and retains strict base64 padding and tail-bit checks. Verification, restore and
migration input, and search/view scans share this validation path. Parsing,
export encoding, BLOB conversion for restore and database binding still require
memory proportional to that row, in addition to fixed frame buffers. The limits
are not a promise of a 256 MiB whole-process memory ceiling.

A continuation group contributes **one logical row** to table and archive
counts. The completion digest covers canonical reconstructed rows, not the
continuation envelopes. For hashing, both versions use the version 1 domain and
normalize the header's wire version and record-type list to version 1, with its
timestamp set to zero. Equal logical content under the same archive identity
therefore retains the same digest across versions. Each group's checksum is an
additional consistency check; it does not replace the final archive completion
or provide a signature. Restore, repeated-import comparison, search and view
consume the reconstructed row through the same validation path.

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
when hashing; version 2 also normalizes its wire version and record-type list
as described above. All table and row records are hashed; the completion is not.
Serialization follows the Rust format structs' declared field order, not input
object key order. The digest binds archive identity, storage schema, descriptors, cell
values and omissions, but not export time or incidental JSON whitespace. This
is an integrity checksum, **not** a signature or proof of source authenticity.

## Scope and qualification

Current restoration supports a new database with the exact current canonical
schema, plus opt-in read-only comparison for an identical existing destination.
Explicit indexed restoration additionally rebuilds canonical lexical search, and
`--allow-compatible-schema` admits the reviewed v20 and v21 -> v22 bridges. Merge, any
other cross-schema migration and semantic reconstruction remain outside these
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
That run did **not** qualify the later opt-in indexed-recovery implementation.

Subsequent Linux run `35687395247` at `695c5449` passed every native test stage:
56 archive tests, 14 archive CLI tests, five indexed-recovery admission tests,
29 canonical-service tests, 20 bookmark tests and nine bookmark-CLI tests.
It includes the corrected indexed-recovery search-to-view journey and direct
backup search. Its separate formatting job failed, so the whole workflow was
not green. Full-message backup views and cursor pagination were added afterward
and need their own exact-source execution results. These fixture results do not
establish large-archive RSS, cross-platform, power-loss, Clippy/UBS or full-release
qualification. The targeted workflow records immutable source, lockfile and
binary identities and rejects empty test-filter success.
