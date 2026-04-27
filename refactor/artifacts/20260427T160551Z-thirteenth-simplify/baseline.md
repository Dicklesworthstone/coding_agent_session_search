# Baseline - Thirteenth Simplification Run

Run ID: `20260427T160551Z-thirteenth-simplify`

## Scope

- Skill: `simplify-and-refactor-code-isomorphically`
- Orchestrator skill: `repeatedly-apply-skill`
- Target: `/data/projects/coding_agent_session_search`
- Branch: `main`
- Baseline HEAD: `afe4d507`
- Existing dirty work preserved: `benches/integration_regression.rs`, `src/storage/sqlite.rs`

## Architecture Notes From Preflight

- `cass` indexes local and remote coding-agent histories into frankensqlite as source of truth and frankensearch-derived lexical/semantic assets for search.
- Robot and JSON surfaces are contract-sensitive and pinned by golden tests.
- Lexical and semantic indexes are derived, recoverable assets; SQLite data, bookmarks, TUI state, sources, and `.env` are preserved.
- New SQLite work must use frankensqlite, not rusqlite; this run is intentionally avoiding storage migration surfaces.
- Bare `cass` is forbidden in agent contexts; only `--robot` or `--json` commands are acceptable.

## Baseline Commands

```text
git rev-parse --short HEAD
afe4d507

git branch --show-current
main

git status --short
 M benches/integration_regression.rs
 M src/storage/sqlite.rs

rg --files -g '*.rs' | xargs wc -l | tail -n 1
  398925 total

which tokei; which scc; which ubs; which rch
tokei not found
scc not found
/home/ubuntu/.local/bin/ubs
/home/ubuntu/.local/bin/rch

rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_thirteenth_simplify cargo check --all-targets
passed

cargo fmt --check
failed on pre-existing formatting drift in tests/golden_robot_docs.rs, tests/golden_robot_json.rs, and tests/metamorphic_agent_detection.rs
```

## Candidate Map

| Mission | Candidate shape | Score basis | Risk note |
|---|---|---:|---|
| 1 | Name one env fallback/default chain | 4.0 | Private pure helper, focused test |
| 2 | Table repeated embedder assertions | 10.0 | Test-only matrix, exact fields |
| 3 | Pin scheduler reason strings | 6.0 | Private literal family, exact text tests |
| 4 | Collapse repeated fixture setup | 4.0 | Test-only helper, edge cases remain local |
| 5 | Factor pure numeric clamp/conversion | 6.0 | Boundary test required |
| 6 | Extract local projection helper | 4.0 | Private helper, field parity test |
| 7 | Centralize local error shape | 4.0 | Exact error text/source parity required |
| 8 | Inline one-call private wrapper | 4.0 | Must verify no independent contract |
| 9 | Convert repeated IO test assertions to matrix | 10.0 | Test-only matrix |
| 10 | Final rescan or convergence dashboard | 2.0+ | Touched-area only |

## Fresh-Eyes Baseline Check

- AGENTS.md was read in full: no deletion, no destructive git/filesystem actions, no scripted code rewrites, no new rusqlite, Cargo-only Rust workflow, RCH for heavy checks, and robot-only cass usage are active constraints.
- README.md was read in full: the project architecture is a Rust CLI/TUI over connector discovery, frankensqlite storage, frankensearch indexing, robot JSON contracts, remote sync, model management, and diagnostic/self-healing surfaces.
- Skill docs were read for the mandatory baseline, map, score, prove, collapse, verify, ledger loop.
- Dirty peer work exists and will remain untouched unless a pass explicitly reserves and edits that path.
