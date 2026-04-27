# Recovery Runbook

Note: JSONL import/export commands are planned. This runbook documents the
intended recovery steps once JSONL sync is implemented.

## Symptoms
- DB corruption detected (PRAGMA integrity_check fails)
- JSONL parse failure
- Mismatched version markers

## Steps
1. Acquire sync lock (<data_dir>/sync.lock)
2. Validate source of truth (SQLite for normal ops)
3. Rebuild target store (JSONL from DB, or DB from JSONL if DB is corrupt)
4. Update version markers in both stores
5. Verify counts/hashes
6. Release lock

## Commands (planned)
- cass export-jsonl  # SQLite -> JSONL snapshot
- cass import-jsonl  # JSONL -> SQLite rebuild
