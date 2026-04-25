# Third Simplification Run Baseline

Run id: `20260425T154730Z-third-simplify`

## Starting Point

- Branch: `main`
- Recent setup base: `940f0f70 skill-loop: finalize second simplification dashboard`
- Rust file inventory under `src`, `tests`, and `benches`: 697 files
- Crude line count under `src`, `tests`, and `benches`: 826,299 total lines

## Dirty Worktree At Start

Unrelated paths present before this run and intentionally excluded from commits:

- `.beads/issues.jsonl`
- `.beads/last-touched`
- `benches/integration_regression.rs`
- `src/indexer/mod.rs`
- `src/storage/sqlite.rs`
- `fuzz/fuzz/`

## Baseline Verification

- Passed: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_third_simplify cargo check --all-targets`
- Known residual: full `cargo fmt --check` remains red on pre-existing formatting drift in `tests/golden_robot_docs.rs`, `tests/golden_robot_json.rs`, and `tests/metamorphic_agent_detection.rs`.

## Candidate Scan Seed

- Trait/default/type-alias scan found safe-looking private surfaces in `src/html_export`, `src/pages`, `src/analytics`, `src/search`, `src/sources`, and `src/ui`.
- `src/indexer/*` and `src/storage/*` candidates are rejected for this run because of unrelated dirty work in those boundaries.
