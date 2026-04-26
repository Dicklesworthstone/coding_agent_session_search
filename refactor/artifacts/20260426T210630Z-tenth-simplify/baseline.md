# Tenth Simplification Loop Baseline

- Run id: `20260426T210630Z-tenth-simplify`
- Baseline HEAD: `97125690`
- Target: `/data/projects/coding_agent_session_search`
- Skill: `simplify-and-refactor-code-isomorphically`
- Loop: strict serial fallback for `repeatedly-apply-skill`
- Started: `2026-04-26T21:06:30Z`

## Project Contract Read

- Re-read all of `AGENTS.md`.
- Re-read all of `README.md`.
- Refactor constraints in force: no file deletion, no script-based code rewrites, no new `rusqlite`, no bare `cass`, preserve peer work, keep robot/golden contracts stable unless intentionally changed.

## Baseline Checks

- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_tenth_simplify cargo check --all-targets` passed.
- `cargo fmt --check` failed only on the known pre-existing formatting drift in:
  - `tests/golden_robot_docs.rs`
  - `tests/golden_robot_json.rs`
  - `tests/metamorphic_agent_detection.rs`
- `tokei` and `scc` were unavailable in this environment.
- Tracked Rust LOC snapshot: `git ls-files 'src/**/*.rs' 'tests/**/*.rs' 'benches/**/*.rs' | xargs wc -l | tail -1` -> `240838 total`.

## Architecture Notes Refreshed

- SQLite is the durable source of truth; lexical and semantic assets are rebuildable derivatives.
- Lexical publish must preserve atomic-swap and quarantine/backup semantics.
- Hybrid search must fail open to lexical with truthful robot metadata.
- Robot JSON schema surfaces are golden-frozen; no schema changes are planned for this loop.
- Remote-source sync is additive-only and provenance-aware.
- HTML export, TUI, model install, update checks, and config writes rely on atomic file patterns.

## Initial Candidate Map

The first architecture search found safe low-risk seams around:

- `src/model/types.rs` tests with repeated roundtrip fixtures and assertion groups.
- `src/indexer/mod.rs` small pure helpers and progress snapshot values.
- Additional passes will rescan before editing and choose only score >= 2.0 candidates.

## Baseline Verdict

Proceed. The code compiles before the tenth simplification loop, and the formatter failure is pre-existing and outside this run's owned files.
