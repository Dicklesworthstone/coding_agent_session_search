# Third Simplification Run Architecture Note

Run id: `20260425T154730Z-third-simplify`

## Documentation Context

- `AGENTS.md` remains authoritative for repo-local safety rules: no file deletion, no destructive git/filesystem commands, no new `rusqlite`, no script-based code rewrites, no bare `cass`, and Cargo verification through repo-appropriate command shapes.
- `README.md` frames cass as a Rust CLI/TUI and robot-mode API for indexing and searching local coding-agent sessions across providers, with SQLite/frankensqlite as durable truth and lexical/semantic search assets as derived state.

## Current Architecture Map

- `src/main.rs`: process entrypoint. It loads dotenv configuration, detects robot output mode, applies resource defaults, chooses the asupersync runtime flavor, parses CLI args, and delegates to `coding_agent_search::run_with_parsed`.
- `src/lib.rs`: large Clap command surface and dispatch hub. It owns robot JSON contracts, friendly parse errors, command normalization, search/index/status/model/pages command routing, and many golden-pinned behaviors.
- `src/connectors/` plus `franken_agent_detection`: provider-specific session discovery and normalization into conversation/message models.
- `src/indexer/`: scan, salvage, canonical storage writes, lexical publish/rebuild, semantic backfill, and progress/health instrumentation. This is high-risk and currently dirty, so this loop avoids it.
- `src/storage/`: frankensqlite-backed source of truth with legacy rusqlite debt. This is high-risk and currently dirty, so this loop avoids it.
- `src/search/`: query parsing, lexical/hybrid/semantic search, model management, daemon client, and robot metadata. Changes here require focused tests and schema caution.
- `src/pages/`, `src/html_export/`: export, encryption, bundle, preview, deployment, profiles, and browser-facing static assets. Good source of bounded helpers and private trait boilerplate.
- `src/analytics/`: usage rollups, query/reporting surfaces, validation, and UI chart data. Good source of local projection or repeated validation simplifications when tests are available.
- `src/ui/`: TUI state, rendering, data cache, charts, theme, and component helpers. High volume, but private formatting/fixture helpers can be safe when well-tested.
- `tests/`: integration and golden contracts. Good for DRYing repeated fixture code only if test names/assertions stay unchanged.

## Avoid List For This Run

- `src/indexer/mod.rs`: unrelated dirty changes present at run start.
- `src/storage/sqlite.rs`: unrelated dirty changes present at run start.
- `.beads/*`, `benches/integration_regression.rs`, `fuzz/fuzz/`: unrelated dirty paths present at run start.
- Full `cargo fmt --check` is known to be red on pre-existing formatting in `tests/golden_robot_docs.rs`, `tests/golden_robot_json.rs`, and `tests/metamorphic_agent_detection.rs`; do not reformat those unless the user widens scope.

## Safe Simplification Strategy

- Prefer private derive replacements, local helpers with three or more identical callsites, constant literal consolidation inside one module, and test fixture helpers.
- One lever per pass, one commit per pass.
- Each pass must include a proof card and the requested fresh-eyes review prompt before closeout.
