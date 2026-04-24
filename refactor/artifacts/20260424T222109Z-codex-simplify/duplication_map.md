# Duplication Map — 20260424T222109Z-codex-simplify

Generated: 2026-04-24 22:21 UTC
Tools run: (none installed)
Raw outputs: refactor/artifacts/20260424T222109Z-codex-simplify/scans/

## How to fill this in

1. Read the scan outputs above.
2. Cluster similar findings into candidates (assign IDs D1, D2, …).
3. For each candidate, fill the table row below.
4. Pass to score_candidates.py.

| ID  | Kind | Locations | LOC each | × | Type | Notes |
|-----|------|-----------|----------|---|------|-------|
| D1  | hand-written error trait boilerplate | `src/html_export/encryption.rs`, `src/html_export/template.rs`, `src/html_export/renderer.rs` | 10-13 | 3 | II | Same `Display` + empty `Error` impl shape; `thiserror` already available. |
