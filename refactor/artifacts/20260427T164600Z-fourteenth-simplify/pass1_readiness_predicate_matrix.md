# Pass 1 - Readiness Predicate Matrix

- Mission: Readiness Label Matrix
- Files changed: `src/search/readiness.rs`
- Commit: pending

## Change

Converted the repeated boolean assertions in:

- `is_searchable_distinguishes_lexical_failure_modes`
- `semantic_can_refine_only_when_at_least_fast_tier_ready`

into explicit `(state, expected)` matrices.

## Isomorphism Check

- Production code unchanged.
- All five lexical readiness states remain covered once.
- All five semantic readiness states remain covered once.
- The expected boolean for each state matches the removed assertion polarity.
- Added per-row diagnostics with `{state:?}` so a failed case still names the exact enum variant.

## Fresh-Eyes Review

Prompt applied: "Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with \"fresh eyes\" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?"

Yes. I re-read both matrices against the removed assertions and verified the false/true polarity was preserved for every state. No follow-up fix was needed.

## Verification

- `rustfmt --edition 2024 --check src/search/readiness.rs`
- `git diff --check -- src/search/readiness.rs refactor/artifacts/20260427T164600Z-fourteenth-simplify/pass1_readiness_predicate_matrix.md`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_fourteenth_simplify cargo test --lib search::readiness::tests::`
- `ubs src/search/readiness.rs refactor/artifacts/20260427T164600Z-fourteenth-simplify/pass1_readiness_predicate_matrix.md` reported no critical issues.
