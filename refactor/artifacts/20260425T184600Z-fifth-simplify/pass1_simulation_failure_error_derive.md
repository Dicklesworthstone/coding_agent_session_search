# Pass 1 - SimulationFailure Error Derive

## Change

Replace the hand-written `Display` and empty `Error` implementations for the
test-harness `SimulationFailure` enum with `thiserror::Error` derive attributes.

## Isomorphism Card

- Inputs covered: `Crash` and `InjectedError` variants in
  `tests/util/search_asset_simulation.rs`.
- Ordering preserved: N/A; formatting is a single match-equivalent string per
  variant.
- Tie-breaking: N/A.
- Error semantics: display strings are pinned exactly; `Error::source()` remains
  `None` for both variants.
- Laziness: unchanged.
- Short-circuit eval: N/A.
- Floating-point: N/A.
- RNG / hash order: N/A.
- Observable side-effects: none.
- Type narrowing: enum variants, serde tags, and equality derives are unchanged.

## Fresh-Eyes Prompt

`Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with "fresh eyes" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?`

## Fresh-Eyes Result

I re-read both new `#[error(...)]` attributes against the removed formatter:

- `Crash` still formats `simulated crash at {failpoint.as_str()}`.
- `InjectedError` still formats
  `simulated failure at {failpoint.as_str()}: {reason}`.
- No field is marked as `#[source]` or `#[from]`, and the targeted test asserts
  `Error::source()` remains `None`.
- The serde representation and public variant fields did not change.

No further fix was needed after the rustfmt shape correction.

## Verification

- `rustfmt --edition 2024 --check tests/util/search_asset_simulation.rs tests/search_asset_simulation.rs`
- `git diff --check -- tests/util/search_asset_simulation.rs tests/search_asset_simulation.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_fifth_simplify cargo test --test search_asset_simulation simulation_failure_display_and_source_are_preserved`

All passed.
