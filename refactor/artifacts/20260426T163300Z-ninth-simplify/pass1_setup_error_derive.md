# Pass 1 - SetupError derive parity

- Run: `20260426T163300Z-ninth-simplify`
- Skill: `simplify-and-refactor-code-isomorphically`
- Mission: Derive/Boilerplate Helper
- Scope: `src/sources/setup.rs`
- Verdict: PRODUCTIVE

## Change

Converted `SetupError` from a manual `Display` implementation plus empty
`std::error::Error` implementation to `thiserror::Error` derive attributes.

## Isomorphism Card

Preserved behavior:

- `Io`, `Json`, `Config`, `Install`, `Index`, and `Interactive` variants keep
  their original `<kind> error: {inner}` display strings.
- `Cancelled`, `NoHosts`, and `Interrupted` keep their exact static messages.
- `std::error::Error::source()` remains `None` for the covered variants, matching
  the previous empty `Error` implementation.
- All construction sites still use the same enum variants.

## Fresh-Eyes Review

Re-read every `#[error(...)]` attribute against the removed match arms. The only
behavioral risk was source chaining, so I added an explicit no-source parity test
for representative unit and wrapped variants.

Yes, preservation was verified according to the skill: exact existing display
tests still pass, and no-source behavior is now pinned.

## Verification

- `rustfmt --edition 2024 --check src/sources/setup.rs`
- `git diff --check -- src/sources/setup.rs`
- `rch exec -- env CARGO_TARGET_DIR=/tmp/rch_target_cass_ninth_simplify cargo test --lib sources::setup::tests::test_setup_error`

