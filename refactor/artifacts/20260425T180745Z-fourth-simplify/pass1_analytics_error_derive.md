# Pass 1/10 - Error Derive Parity

## Isomorphism Card

### Change

Replace hand-written `AnalyticsError` `Display` plus empty `Error` impls with `thiserror::Error` derive annotations in `src/analytics/types.rs`.

### Equivalence Contract

- Error variants: unchanged. `MissingTable(String)` and `Db(String)` remain the only variants.
- Display text: unchanged and pinned by test.
- Source chaining: unchanged. Neither variant exposes a source error before or after the derive.
- Public type alias: unchanged. `AnalyticsResult<T>` still aliases `Result<T, AnalyticsError>`.
- Serialization/API output: unchanged. `AnalyticsError` is not serialized here; callers still use `to_string()` for display.

### Candidate Score

- LOC saved: 14
- Confidence: 5
- Risk: 1
- Score: 70.0
- Decision: accept. This is a direct derive replacement with exact strings pinned.

## Files Changed

- `src/analytics/types.rs`: derived `thiserror::Error` for `AnalyticsError` and added display/source parity coverage.
- `refactor/artifacts/20260425T180745Z-fourth-simplify/pass1_analytics_error_derive.md`: this proof card.

## Fresh-Eyes Review

Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with "fresh eyes" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?

- Re-read both derive attributes against the removed `Display` match arms and confirmed the strings are byte-for-byte identical.
- Confirmed no `#[source]`, `#[from]`, or `#[transparent]` attribute was added, preserving `source() == None`.
- Added a parity test for both variants to verify display text and source behavior.

## Verification

- Passed: `rustfmt --edition 2024 --check src/analytics/types.rs`
- Passed: `git diff --check -- src/analytics/types.rs refactor/artifacts/20260425T180745Z-fourth-simplify/pass1_analytics_error_derive.md .skill-loop-progress.md`
- Passed: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_fourth_simplify cargo test --lib analytics::types::tests::analytics_error_display_and_sources_are_preserved`
