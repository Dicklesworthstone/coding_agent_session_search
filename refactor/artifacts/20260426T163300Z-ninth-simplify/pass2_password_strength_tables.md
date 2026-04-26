# Pass 2 - PasswordStrength assertion tables

- Run: `20260426T163300Z-ninth-simplify`
- Skill: `simplify-and-refactor-code-isomorphically`
- Mission: Assertion Table
- Scope: `src/pages/password.rs`
- Verdict: PRODUCTIVE

## Change

Converted repeated `PasswordStrength::bar()` and `PasswordStrength::percent()`
assertions into table-driven loops.

## Isomorphism Card

Preserved behavior:

- `Weak`, `Fair`, `Good`, and `Strong` remain covered for bar rendering.
- `Weak`, `Fair`, `Good`, and `Strong` remain covered for percent values.
- Expected bar strings and percentages are unchanged.
- The change is test-only and does not touch password scoring or display logic.

## Fresh-Eyes Review

Re-read both converted tests against the removed assertions. Every previous
case is present, and each assertion now includes `{strength:?}` so failures
remain easy to identify.

Yes, preservation was verified according to the skill: the focused bar and
percent tests both passed.

## Verification

- `rustfmt --edition 2024 --check src/pages/password.rs`
- `git diff --check -- src/pages/password.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_ninth_simplify cargo test --lib pages::password::tests::test_strength_`

