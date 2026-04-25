# Pass 2/10 - Enum String Helper

## Isomorphism Card

### Change

Extract `Metric::as_str()` as the stable string source of truth for analytics metric names, and delegate `Display` to it.

### Equivalence Contract

- Metric variants: unchanged.
- Display strings: unchanged for all 12 variants.
- Serde names: unchanged; the enum still uses `#[serde(rename_all = "snake_case")]`.
- Query behavior: unchanged. `rollup_column()` and query match arms were not altered.
- Public JSON shape: unchanged. Existing `to_string()` callers receive the same strings.

### Candidate Score

- LOC saved: small, but removes a duplicated formatting responsibility.
- Confidence: 5
- Risk: 1
- Score: 2.5
- Decision: accept. A table test now pins all strings, including variants not covered by the older query test.

## Files Changed

- `src/analytics/types.rs`: added `Metric::as_str()`, made `Display` delegate to it, and added exhaustive string/display parity coverage.
- `refactor/artifacts/20260425T180745Z-fourth-simplify/pass2_metric_as_str.md`: this proof card.

## Fresh-Eyes Review

Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with "fresh eyes" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?

- Re-read every `Metric` variant in declaration order against the old `Display` match arms.
- Confirmed the `EstimatedCostUsd` and `ContentEstTotal` spellings stayed snake_case and did not drift from serde output expectations.
- Left `rollup_column()` unchanged because SQL column names intentionally differ from display names.

## Verification

- Passed: `rustfmt --edition 2024 --check src/analytics/types.rs`
- Passed: `git diff --check -- src/analytics/types.rs refactor/artifacts/20260425T180745Z-fourth-simplify/pass2_metric_as_str.md .skill-loop-progress.md`
- Passed: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_fourth_simplify cargo test --lib analytics::types::tests::metric_as_str_matches_display_for_all_variants`
