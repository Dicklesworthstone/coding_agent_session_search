# Pass 5 - Pure Conversion Clamp

## Change

Factor repeated semantic progress counter `usize` to `u64` casts into `saturating_u64_from_usize(...)`.

## Score

| LOC saved | Confidence | Risk | Score |
|---:|---:|---:|---:|
| 2 | 5 | 1 | 10.0 |

## Equivalence Contract

- Inputs covered: progress lengths from `batch.len()`, `messages.len()`, and `skipped_in_window`.
- Ordering preserved: progress increments happen at the same call sites.
- Tie-breaking: N/A.
- Error semantics: unchanged; conversion is infallible.
- Laziness: unchanged.
- Short-circuit eval: N/A.
- Floating-point: N/A.
- RNG/hash order: N/A.
- Observable side effects: progress bar totals and increments preserve prior values on supported targets; overflow would now saturate instead of truncating on any wider future target.
- Type narrowing: conversion is now explicit and named.

## Fresh-Eyes Review

I re-read the three converted progress call sites. The helper receives the same `usize` values the casts used before, and each call still updates the same progress bar in the same location. The boundary test pins zero, ordinary values, and platform `usize::MAX` behavior.

## Verification

- Passed: `rustfmt --edition 2024 --check src/indexer/semantic.rs`
- Passed: `git diff --check -- src/indexer/semantic.rs refactor/artifacts/20260427T160551Z-thirteenth-simplify/pass5_semantic_progress_u64_conversion.md`
- Passed: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_thirteenth_simplify cargo test --lib indexer::semantic::tests::saturating_u64_from_usize_covers_bounds -- --exact`
