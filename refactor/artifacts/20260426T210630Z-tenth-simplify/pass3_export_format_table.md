# Pass 3 - Export Format String Table

## Mission

Centralize one small local string mapping with exact display and cycle parity.

## Change

Added a private `ExportFormat::metadata()` helper that holds the display name, file extension, and next-format transition for each export format in one match.

`name()`, `extension()`, and `next()` now project from that shared metadata instead of maintaining three parallel matches.

## Isomorphism Card

- Inputs covered: all `ExportFormat` variants.
- Ordering preserved: `Markdown -> Json -> PlainText -> Markdown` is unchanged.
- Tie-breaking: N/A.
- Error semantics: unchanged; all methods are infallible and still return static values.
- Laziness: unchanged.
- Short-circuit eval: N/A.
- Floating-point: N/A.
- RNG / hash order: N/A.
- Observable side effects: unchanged; pure methods only.
- Robot JSON / public contracts: unchanged; export payload generation code was not modified.

## Fresh-Eyes Review

Compared each tuple in `metadata()` against the removed matches:

- `Markdown`: name `Markdown`, extension `md`, next `Json`.
- `Json`: name `JSON`, extension `json`, next `PlainText`.
- `PlainText`: name `Plain Text`, extension `txt`, next `Markdown`.

No bug or semantic drift found.

## Verification

- `rustfmt --edition 2024 --check src/export.rs src/model/types.rs`
- `git diff --check -- src/export.rs .skill-loop-progress.md refactor/artifacts/20260426T210630Z-tenth-simplify/pass3_export_format_table.md`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_tenth_simplify cargo test --lib test_export_format`

## LOC Delta

- `src/export.rs`: 11 insertions, 15 deletions.
- Net: -4 lines.

## Verdict

PRODUCTIVE. The pass removed parallel literal matches while preserving the export-format cycle and extension contract.
