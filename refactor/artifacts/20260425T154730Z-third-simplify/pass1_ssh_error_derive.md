# Pass 1/10 - Private Trait Boilerplate

## Isomorphism Card

### Change

Replace hand-written `Display` plus empty `Error` implementations for `SshTestError` in `tests/ssh_test_helper.rs` with `thiserror::Error` derive annotations.

### Equivalence Contract

- Inputs covered: every `SshTestError` variant used by `ssh_sync_integration`.
- Display text: unchanged. Each `#[error(...)]` string is copied from the previous `write!` branch.
- Error source behavior: unchanged. The previous `Error` impl did not override `source()`, and no variant is annotated with `#[source]` or `#[from]`.
- Public API / schema: unchanged. This is a test helper module used by one integration-test target.
- Runtime side effects: unchanged. Only generated trait boilerplate replaces manual formatting code.

### Candidate Score

- LOC saved: 10
- Confidence: 5
- Risk: 1
- Score: 50.0
- Decision: accept. This is isolated test-helper boilerplate with exact string preservation.

## Files Changed

- `tests/ssh_test_helper.rs`: derive `thiserror::Error` for `SshTestError`.
- `refactor/artifacts/20260425T154730Z-third-simplify/pass1_ssh_error_derive.md`: this proof card.

## Fresh-Eyes Review

Great, now I want you to carefully read over all of the new code you just wrote and other existing code you just modified with "fresh eyes" looking super carefully for any obvious bugs, errors, problems, issues, confusion, etc. Carefully fix anything you uncover. Did you actually verify that everything was preserved according to the skill?

- Re-read every old display branch against the new `#[error(...)]` attributes.
- Confirmed there is no source-bearing field, so source-chain behavior remains `None`.
- Confirmed the helper remains test-only and no production or robot-mode error surface changed.

## Verification

- Passed: `rustfmt --edition 2024 --check tests/ssh_test_helper.rs`
- Passed: `git diff --check -- tests/ssh_test_helper.rs refactor/artifacts/20260425T154730Z-third-simplify/pass1_ssh_error_derive.md`
- Passed: `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_third_simplify cargo test --test ssh_sync_integration --no-run`
