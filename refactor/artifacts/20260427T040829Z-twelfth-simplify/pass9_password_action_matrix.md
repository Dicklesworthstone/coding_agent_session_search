# Pass 9 - Password Action Parsing Matrix

## Target

- File: `src/pages/confirmation.rs`
- Seam: `test_password_action_parsing`

## Simplification

Converted three repeated password-action parser assertions into one explicit input/expected matrix.

## Isomorphism Card

- Preserved `s -> SetStronger`.
- Preserved uppercase `P -> ProceedAnyway`, which keeps case-normalization coverage.
- Preserved `a -> Abort`.
- Preserved the separate invalid `x -> None` assertion.
- Added `input=...` diagnostics for matrix failures.

## Fresh-Eyes Review

Re-read the matrix against the removed assertions and the parser implementation. No input or expected enum variant changed, and the invalid-input case remains outside the success matrix.

## Verification

- `rustfmt --edition 2024 --check src/pages/confirmation.rs`
- `git diff --check -- src/pages/confirmation.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_twelfth_simplify cargo test --lib pages::confirmation::tests::test_password_action_parsing`

## Verdict

PRODUCTIVE. The test is shorter while preserving all parser coverage and case diagnostics.
