# Pass 8/10 - Data Projection Helper

## Isomorphism Card

### Change

Extract private `breakdown_row_with_value` in `src/analytics/query.rs` for repeated `BreakdownRow` projection after each callsite has already computed its metric-specific `value`.

### Equivalence Contract

- Inputs covered: four breakdown row construction sites in Track A/Track B breakdown paths.
- Ordering preserved: yes. Row iteration, sorting, and truncation are unchanged.
- Metric semantics: unchanged. Every match on `Metric` remains at its original callsite, including Track A raw `PlanCount`, Track B `PlanCount`, coverage percentage, and cost fallbacks.
- Field mapping: unchanged. `key`, `value`, `message_count: bucket.message_count`, and `bucket` move into the same `BreakdownRow` fields.
- Serde field order: unchanged because it is determined by the `BreakdownRow` struct definition, not construction-site field order.
- Public API / schema: unchanged. Helper is private.

### Candidate Score

- LOC saved: 8
- Confidence: 5
- Risk: 1
- Score: 40.0
- Decision: accept. The helper removes repeated struct projection while leaving metric-specific behavior local and explicit.

## Files Changed

- `src/analytics/query.rs`: added private `breakdown_row_with_value` and replaced four repeated `BreakdownRow` projections.
- `refactor/artifacts/20260425T024205Z-second-simplify/pass8_breakdown_row_projection.md`: this proof card.

## Fresh-Eyes Review

Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with "fresh eyes" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?

- Re-read all four callsites after extraction.
- Confirmed the helper does not know about `Metric`, so no metric-specific fallback moved or changed.
- Confirmed `message_count` is still copied from the same bucket before the bucket is moved into the row.
- Confirmed sorting still reads `row.value` and `row.key` exactly as before.

## Verification

- `rustfmt --edition 2024 --check src/analytics/query.rs`
  - Result: passed with no output.
- `git diff --check -- src/analytics/query.rs refactor/artifacts/20260425T024205Z-second-simplify/pass8_breakdown_row_projection.md`
  - Result: passed with no output.
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_second_simplify cargo test --lib analytics::query::`
  - Result: passed, `121 passed; 0 failed`.
