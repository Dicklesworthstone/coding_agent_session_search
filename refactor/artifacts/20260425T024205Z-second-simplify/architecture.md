# Architecture Preflight

Run id: `20260425T024205Z-second-simplify`

## Sources Read
- `AGENTS.md` in full: 1166 lines.
- `README.md` in full: 2867 lines.
- `repeatedly-apply-skill`, `simplify-and-refactor-code-isomorphically`, and `codebase-archaeology` skill files.
- `Cargo.toml`, `src/main.rs`, `src/lib.rs`, `src/connectors/mod.rs`, `src/model/types.rs`, `src/storage/sqlite.rs`, `src/search/query.rs`, and repository file inventories.

## Project Model
- `cass` is a Rust 2024 single-crate CLI/TUI for indexing coding-agent histories into a unified archive and search surface.
- `src/main.rs` loads `.env`, enforces the AVX preflight, applies the Tantivy writer thread cap, parses CLI args via `parse_cli`, and runs `run_with_parsed` on asupersync.
- `src/lib.rs` owns Clap command definitions, robot-mode output contracts, command dispatch, health/status/doctor/search/session/export/source flows, and stable error envelopes.
- Connector modules in `src/connectors/` are compatibility re-export stubs over `franken_agent_detection`; normalized conversations are mapped into internal `Conversation`, `Message`, and `Snippet` types.
- `src/storage/sqlite.rs` is the frankensqlite source of truth and contains lazy DB open, migration, archive, analytics, and rebuild helpers. This file is currently dirty from peer work and is excluded from this loop unless explicitly re-evaluated.
- `src/indexer/` discovers/scans sources, persists canonical archive rows, and rebuilds derived lexical/semantic assets. Lexical publish uses atomic swap/retention semantics and must not be bypassed.
- `src/search/` wraps frankensearch lexical/semantic/two-tier machinery, cache behavior, readiness, and robot metadata. Hybrid search must fail open to lexical with truthful metadata.
- `src/ui/` and `src/pages/` are broad presentation surfaces. Refactors here must preserve visible strings, key bindings, state persistence, and serde/robot field names.

## Refactor Invariants
- No file deletion. No destructive git/filesystem commands.
- No new `rusqlite`; new SQLite code must use frankensqlite.
- Do not touch current peer-dirty files unless the pass explicitly owns them: `.beads/issues.jsonl`, `benches/integration_regression.rs`, `fuzz/fuzz_targets/fuzz_query_transpiler.rs`, `src/html_export/scripts.rs`, `src/storage/sqlite.rs`, or `fuzz/fuzz/`.
- Keep public robot JSON schemas, golden outputs, error `kind` values, exit codes, CLI stdout/stderr split, and lexical publish semantics unchanged.
- One simplification lever per pass; each pass gets its own commit and fresh-eyes review.
