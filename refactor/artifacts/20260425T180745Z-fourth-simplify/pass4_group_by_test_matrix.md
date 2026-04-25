# Pass 4/10 - Test Matrix Consolidation

## Isomorphism Card

### Change

Replaced repeated `GroupBy` enum assertions with one shared `GROUP_BY_CASES` table covering display text, label text, next-cycle, and previous-cycle behavior.

### Equivalence Contract

- Test coverage: unchanged for all four variants.
- Display strings: unchanged: `hour`, `day`, `week`, `month`.
- Labels: unchanged: `Hourly`, `Daily`, `Weekly`, `Monthly`.
- Cycle behavior: unchanged for `next()` and `prev()`.
- Production code: unchanged.

### Candidate Score

- LOC saved: modest, but the enum matrix now has one source of expected behavior.
- Confidence: 5
- Risk: 1
- Score: 3.0
- Decision: accept. This is test-only and improves future variant-audit reliability.

## Files Changed

- `src/analytics/types.rs`: added `GROUP_BY_CASES` and rewired existing `GroupBy` tests through it.
- `refactor/artifacts/20260425T180745Z-fourth-simplify/pass4_group_by_test_matrix.md`: this proof card.

## Fresh-Eyes Review

Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with "fresh eyes" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?

- Re-read every row against the prior assertions and confirmed all 16 expected values are still checked.
- Kept the existing test functions so targeted `group_by_*` test filtering still works.
- Added `Debug` context to loop assertions so failures still identify the variant.
- Fixed the array-row wrapping reported by the first `rustfmt --check`.

## Verification

- Passed after applying the rustfmt row-wrap fix: `rustfmt --edition 2024 --check src/analytics/types.rs`
- Passed: `git diff --check -- src/analytics/types.rs refactor/artifacts/20260425T180745Z-fourth-simplify/pass4_group_by_test_matrix.md .skill-loop-progress.md`
- Passed: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_fourth_simplify cargo test --lib analytics::types::tests::group_by`
