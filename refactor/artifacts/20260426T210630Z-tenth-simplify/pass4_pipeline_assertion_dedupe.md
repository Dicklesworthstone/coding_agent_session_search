# Pass 4 - Pipeline Assertion Dedupe

## Mission

Convert one repeated assertion group into an equivalent smaller check without reducing diagnostics.

## Change

Removed a duplicate assertion block from `state_meta_json_reports_lexical_rebuild_pipeline_settings`.

The removed block repeated the same five `pipeline[...]` checks already asserted earlier in the same test:

- `controller_mode`
- `controller_restore_clear_samples`
- `controller_restore_hold_ms`
- `controller_loadavg_high_watermark_1m`
- `controller_loadavg_low_watermark_1m`

## Isomorphism Card

- Inputs covered: `state_meta_json_reports_lexical_rebuild_pipeline_settings`.
- Ordering preserved: the surviving assertions still run before the staged worker and batch-size assertions.
- Tie-breaking: N/A.
- Error semantics: unchanged; no production code was touched.
- Laziness: unchanged.
- Short-circuit eval: unchanged.
- Floating-point: unchanged; the `7.5` and `6.25` checks remain.
- RNG / hash order: N/A.
- Observable side effects: unchanged; test-only assertion removal.
- Robot JSON / public contracts: unchanged.

## Fresh-Eyes Review

Re-read the deleted block against the surviving block above it. The assertions were byte-for-byte equivalent in key, accessor, and expected value except for formatting context. No unique assertion was removed.

## Verification

- `rustfmt --edition 2024 --check src/lib.rs src/export.rs`
- `git diff --check -- src/lib.rs .skill-loop-progress.md refactor/artifacts/20260426T210630Z-tenth-simplify/pass4_pipeline_assertion_dedupe.md`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_tenth_simplify cargo test --lib state_meta_json_reports_lexical_rebuild_pipeline_settings`

## LOC Delta

- `src/lib.rs`: 14 deletions.
- Net: -14 lines.

## Verdict

PRODUCTIVE. The pass removed duplicate test assertions without weakening the covered values.
