# Pass 7 - Shared DB-ID Helpers

- Mission: Small Pure Helper.
- Files changed: `src/indexer/semantic.rs`, `src/daemon/worker.rs`.
- Simplification: reused the semantic indexer's existing DB-id conversion and saturating `u32` helpers in the daemon worker instead of keeping identical private copies.
- Isomorphism proof: helper bodies are unchanged; only visibility and call-site ownership changed. Existing daemon tests still exercise negative, zero, positive, and overflow boundaries through the imported helpers.
- Fresh-eyes review: verified no SQL/query logic moved, no error handling changed, and daemon comments around corrupted IDs and clamping still describe the same behavior.
