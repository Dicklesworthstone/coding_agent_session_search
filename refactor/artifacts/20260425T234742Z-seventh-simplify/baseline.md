# Baseline - Seventh Simplification Run

- Run: `20260425T234742Z-seventh-simplify`
- Skill: `simplify-and-refactor-code-isomorphically`
- Repetition mode: `repeatedly-apply-skill`, fallback serial mode without spawned subagents
- Agent Mail identity: `FrostySpire`
- Target: `/data/projects/coding_agent_session_search`

## Docs And Architecture Read

- Read local `AGENTS.md` completely.
- Read local `README.md` completely.
- Read `repeatedly-apply-skill/SKILL.md`.
- Read `simplify-and-refactor-code-isomorphically/SKILL.md`.
- Read memory notes for prior cass simplification loops.
- Ran Morph codebase search over the current architecture and safe simplification seams.

## Architecture Summary

`cass` is a Rust 2024 single-crate CLI/TUI. `src/main.rs` handles AVX preflight, robot-mode fatal-error routing, Tantivy writer defaults, CLI parsing, and the asupersync runtime. `src/lib.rs` owns the clap command surface and dispatch. Core subsystems are:

- `connectors/`: normalize local and remote agent session formats.
- `storage/`: frankensqlite-backed canonical archive and migrations.
- `indexer/`: indexing, lexical publish, semantic catch-up, and search asset repair.
- `search/`: lexical/semantic/hybrid query support and model management.
- `pages/` and `html_export/`: static archive generation, encryption, deploy, preview, and key management.
- `sources/`: SSH source setup/sync/provenance.
- `ui/`: FrankenTUI app surface.

## Baseline Verification

- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_seventh_simplify cargo check --all-targets` passed.
- `cargo fmt --check` is red only in known unrelated files:
  - `tests/golden_robot_docs.rs`
  - `tests/golden_robot_json.rs`
  - `tests/metamorphic_agent_detection.rs`

## Existing Dirty Work Preserved

The new run starts with unrelated dirty peer edits in:

- `src/indexer/mod.rs`
- `src/storage/sqlite.rs`

These are explicitly out of scope for this loop unless the user redirects.

## Verification Shape

For each pass:

1. One narrow isomorphic change.
2. Proof card under this artifact directory.
3. Touched-file `rustfmt --edition 2024 --check`.
4. `git diff --check` on touched files.
5. One focused behavioral test.
6. Fresh-eyes reread and fix any issue before commit.
7. One commit per pass.
