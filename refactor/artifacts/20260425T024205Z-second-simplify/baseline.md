# Baseline

Run id: `20260425T024205Z-second-simplify`

## Initial State
- Branch: `main`, ahead of `origin/main` by existing skill-loop commits.
- Existing previous loop: `.skill-loop-progress.md` recorded `20260424T230127Z-repeated-simplify` as complete through pass 10.
- Current unrelated dirty files at loop start:
  - `.beads/issues.jsonl`
  - `benches/integration_regression.rs`
  - `fuzz/fuzz_targets/fuzz_query_transpiler.rs`
  - `src/html_export/scripts.rs`
  - `src/storage/sqlite.rs`
  - `fuzz/fuzz/`

## Metrics
- Crude Rust line-count baseline from `rg --files src tests | rg '\.rs$' | xargs wc -l`: `383331 total`.
- `tokei` and `scc` were not installed in the active shell, so per-pass LOC evidence will use `git diff --stat`, touched-file line counts, and targeted test output.

## Baseline Gates
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_second_simplify cargo check --all-targets` passed before pass 1.
- Full `cargo fmt --check` is known from the prior completed loop to be blocked by unrelated pre-existing formatting drift in `tests/golden_robot_docs.rs`, `tests/golden_robot_json.rs`, and `tests/metamorphic_agent_detection.rs`; this loop will use touched-file rustfmt checks plus final full-gate evidence.

## Exclusions
The loop must avoid local browser E2E and `tests/e2e_large_dataset.rs` for routine gate work unless a pass specifically targets that suite.
