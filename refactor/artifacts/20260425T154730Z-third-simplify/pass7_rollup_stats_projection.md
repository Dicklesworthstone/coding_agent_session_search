# Pass 7/10 - Data Projection Helper

## Isomorphism Card

### Change

Extract `rollup_stats_from_summary_row(...)` in `src/analytics/query.rs` for three repeated `COUNT/MIN/MAX/MAX(updated)` row projections into `RollupStats`.

### Equivalence Contract

- Field order: unchanged. Column `0` maps to `row_count`, `1` to `min_day`, `2` to `max_day`, and `3` to `last_updated`.
- Defaulting behavior: unchanged. The helper preserves `unwrap_or(0)` for `row_count` and `unwrap_or(None)` for the optional fields.
- Query behavior: unchanged. Each callsite still uses the same SQL, parameters, and final `.unwrap_or_default()`.
- Struct visibility: unchanged. `RollupStats` and the helper remain private to the analytics query module.
- Public output: unchanged. This helper only centralizes internal row hydration.

### Candidate Score

- Repeated projections collapsed: 3
- Confidence: 5
- Risk: 1
- Score: 15.0
- Decision: accept. This is a private row projection with identical column indexes and fallback behavior.

## Files Changed

- `src/analytics/query.rs`: extracted the shared `RollupStats` row projection.
- `refactor/artifacts/20260425T154730Z-third-simplify/pass7_rollup_stats_projection.md`: this proof card.

## Fresh-Eyes Review

Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with "fresh eyes" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?

- Re-read all three replaced closures and confirmed each SQL shape selects the same four summary columns.
- Confirmed the projection still suppresses typed-read failures into default values exactly as before.
- Confirmed the helper did not move or alter the outer query failure fallback, which remains `.unwrap_or_default()`.

## Verification

- Passed: `rustfmt --edition 2024 --check src/analytics/query.rs`
- Passed: `git diff --check -- src/analytics/query.rs refactor/artifacts/20260425T154730Z-third-simplify/pass7_rollup_stats_projection.md .skill-loop-progress.md`
- Passed: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_third_simplify cargo test --lib analytics::query::` (121 passed)
