# Pass 3 - Semantic Reason Literals

## Change

Pin every `SemanticBackfillSchedulerReason::next_step()` literal in one explicit test table.

## Score

| LOC saved | Confidence | Risk | Score |
|---:|---:|---:|---:|
| 2 | 5 | 1 | 10.0 |

## Equivalence Contract

- Inputs covered: all eight `SemanticBackfillSchedulerReason` variants.
- Ordering preserved: N/A for production; test table follows enum order.
- Tie-breaking: N/A.
- Error semantics: unchanged; production code is not modified.
- Laziness: N/A.
- Short-circuit eval: N/A.
- Floating-point: N/A.
- RNG/hash order: N/A.
- Observable side effects: unchanged; exact robot-facing next-step strings are now pinned.
- Type narrowing: N/A.

## Fresh-Eyes Review

I re-read the table against the `next_step()` match. Every variant is represented once and every expected string is copied exactly from the production branch. This does not alter behavior; it establishes a guard before future scheduler simplifications.

## Verification

- Passed: `rustfmt --edition 2024 --check src/indexer/semantic.rs`
- Passed: `git diff --check -- src/indexer/semantic.rs refactor/artifacts/20260427T160551Z-thirteenth-simplify/pass3_scheduler_reason_next_steps.md`
- Passed: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_thirteenth_simplify cargo test --lib indexer::semantic::tests::semantic_backfill_scheduler_reason_next_steps_are_stable -- --exact`
