# Pass 2 - Confirmation Strength Label Assertion Table

## Target

- File: `src/pages/confirmation.rs`
- Seam: `test_password_strength_label`

## Simplification

Converted five repeated `assert_eq!(password_strength_label(...), ...)` assertions into one explicit `(entropy_bits, expected_label)` table.

## Isomorphism Card

- Preserved every prior entropy probe: `10.0`, `30.0`, `50.0`, `70.0`, `90.0`.
- Preserved every expected public label: `Very Weak`, `Weak`, `Fair`, `Strong`, `Very Strong`.
- Added the entropy value to the assertion message so a table failure still identifies the exact case.
- Changed only test structure; production code is untouched.

## Fresh-Eyes Review

Re-read the converted table against the removed assertions and surrounding password entropy tests. The table has the same five cases in the same order, no boundary value was dropped, and the new diagnostic string is test-only.

## Verification

- `rustfmt --edition 2024 --check src/pages/confirmation.rs`
- `git diff --check -- src/pages/confirmation.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_twelfth_simplify cargo test --lib pages::confirmation::tests::test_password_strength_label`

## Verdict

PRODUCTIVE. Behavior preserved by exact one-for-one test-case parity and focused test execution.
