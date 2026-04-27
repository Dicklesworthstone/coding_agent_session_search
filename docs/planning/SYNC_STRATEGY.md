# Sync Strategy

Note: JSONL sync is not implemented yet. This document captures the design
so future implementation is consistent with cass's storage model.

## Source of Truth
- Primary: SQLite (agent_search.db)
- JSONL: one-way export snapshot for backup/inspection (not used at runtime)
- Rationale: search/indexing requires SQLite + Tantivy; JSONL is audit/recovery

## Sync Triggers
- On command: planned `cass export-jsonl --out <data_dir>/sessions.jsonl`
- On exit: none (manual only)
- Timer/throttle: none (manual only)

## Versioning
- DB marker: meta keys `jsonl_last_export_ms` and `jsonl_last_export_hash`
- JSONL marker: first line `_meta` record with export_ms/record_count/db_hash

## Concurrency
- Lock file path: <data_dir>/sync.lock
- Busy timeout: 5s for sync routines; no concurrent syncs

## Failure Handling
- DB locked: retry with busy timeout; fail with non-zero if still locked
- JSONL parse error: keep prior JSONL and report; re-export required
- Git commit error: warn and continue; JSONL remains on disk
