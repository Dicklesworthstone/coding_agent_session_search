# Pass 3 - Secret Acknowledgment Literal Family

## Target

- File: `src/pages/confirmation.rs`
- Seam: `ConfirmationFlow::validate_secret_ack`

## Simplification

Named the secret acknowledgment phrase and its normalized comparison spelling as private constants.

## Isomorphism Card

- Accepted phrase remains `I understand the risks` case-insensitively after trim/lowercase normalization.
- Failure text remains exactly `Please type exactly: "I understand the risks"`.
- Constants are private to the module; no public API or JSON surface changed.
- The focused test now asserts the exact failure message instead of only matching any failure.

## Fresh-Eyes Review

Re-read the validation path, the normalized comparison, and the strengthened test. The new constants only remove duplicate spelling of the same phrase; validation order, trimming, lowercasing, and returned `StepValidation` variants are unchanged.

## Verification

- `rustfmt --edition 2024 --check src/pages/confirmation.rs`
- `git diff --check -- src/pages/confirmation.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_twelfth_simplify cargo test --lib pages::confirmation::tests::test_secret_ack_validation`

## Verdict

PRODUCTIVE. Literal drift risk is reduced while exact phrase and error-message behavior are pinned by test output.
