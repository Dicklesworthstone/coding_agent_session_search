# Sync Strategy

## Scope decision — 2026-09-17

Logical JSONL archive export/import is **not implemented**. Preserve the
format-independent portability goal as the open implementation task
`coding_agent_session_search-2l1b0.34`; this document supplies its minimum
contract. `coding_agent_session_search-2l1b0.27` decides scope only and cannot
serve as evidence of a working round trip.

Current capabilities have different boundaries:

| Route | What it covers | What it does not establish |
|---|---|---|
| `cass sources sync` | Mirrors provider source files through the existing remote-source pipeline for local indexing. | Full-fidelity logical export/import of cass's canonical archive. |
| `cass doctor archive export`, `export verify`, and `relocate` | Existing file-copy/manifest verification and relocation paths in `src/lib.rs`; tests include archive-export symlink refusal and external database sidecar discovery. | An engine/schema-independent interchange format, or cross-version import parity. Actual release/platform qualification remains `.25`. |
| Search JSONL, session/HTML/Pages export | Query results or presentation of selected sessions. | A complete restorable canonical archive. |
| `cass import chatgpt` | A provider-specific importer of ChatGPT's `conversations.json`. | A general cass archive importer. |

Those routes remain supported within their own contracts. Do not document
`cass export-jsonl` or `cass import-jsonl` as available commands. Their names
below are proposals, not CLI compatibility promises.

## Source of Truth

- Canonical data lives in SQLite (`agent_search.db`), accessed through FrankenSQLite.
- Lexical and semantic indexes are derived assets and must be rebuilt after import.
- A logical export is an explicit snapshot for interchange/inspection, not a
  second runtime database. A full-fidelity export contains private session data.

## Sync Triggers

- Export and import require explicit operator commands. No startup, exit,
  timer, indexing hook or automatic Git commit is part of this feature.
- The historical one-way export is retained as the first half of a logical
  round trip; import remains separate, explicit and non-destructive on failure.

## Versioning

- First record: format identifier `cass.logical_archive`, schema version `1`, stable archive
  identity, export timestamp and declared record types. Reject unknown versions
  before importing records. Do not use a source pathname as identity.
- Preserve source/provider/session/message identity and canonical conversation,
  message, tool and usage metadata. Destination numeric row IDs may differ;
  relationships and logical identity must survive.
- Include counts and a canonical content digest in a completion record. A
  missing completion record, count mismatch or digest mismatch is an incomplete
  export, not an importable prefix. A raw database-file hash is optional
  provenance, not the definition of logical equality across engines/versions.
- The proposed `jsonl_last_export_ms`/`jsonl_last_export_hash` source-database
  writes are retired. Export must not mutate its source archive; keep export
  metadata in the resulting artifact instead.

## Streaming and import semantics

- Version 1 limits each UTF-8 JSONL record to 8 MiB, including its newline.
  Enforce this limit before allocating an unbounded line buffer. Stream messages
  individually, not as an entire conversation; hold at most one decoded message
  at a time and bound any import batch to 128 records and 16 MiB of encoded input,
  whichever comes first. Working memory must remain independent of archive size.
  Oversized records fail explicitly; never silently truncate bodies or metadata.
- Reimporting the same archive/source/record identities and content is
  idempotent. An existing identity with different content is a reported conflict;
  it cannot silently overwrite the destination.
- Reuse existing storage and staging/publication seams. Validate into isolated
  staging before making imported data visible. An interrupted or rejected import
  leaves the prior destination intact; do not commit an arbitrary valid prefix.
- Do not interpret exported paths as filesystem write destinations or fetch
  external URLs while importing. An explicitly redacted export must advertise
  its omissions and cannot be presented as lossless restoration.

## Concurrency

- Acquire exclusive authority for the export target or import destination using
  the existing lock conventions, with a bounded five-second acquisition deadline.
- Obtain a consistent read snapshot of canonical data. Reject a source that
  cannot supply one; do not mix generations across pages or record categories.
- The historical generic `<data_dir>/sync.lock` is not an implemented shared
  contract with source mirroring. Avoid coupling unrelated source-sync jobs to
  a new logical-export lock.

## Failure Handling

- Locked/busy source or destination: stop at the deadline with a non-zero,
  actionable error; do not retry indefinitely.
- Unknown schema, malformed/truncated/oversized records, duplicate conflicting
  identities or digest failure: retain prior output/destination, report record
  location without echoing private bodies, and publish nothing partial.
- Git commit failure handling is retired because automatic Git commits are out
  of scope. The operator controls distribution of the private export artifact.

## Acceptance for the retained implementation task

`coding_agent_session_search-2l1b0.34` requires a real disposable-archive round trip preserving logical records
and relationships; repeated-import idempotence; conflicting-content and unknown
version refusal; malformed, truncated, oversized and checksum-invalid negatives;
interruption recovery; a bounded-memory large-archive case; and explicit privacy
controls. Run final-source tests through RCH with nonzero terminal counts and
source/binary provenance. No such results are supplied by this scope decision.
